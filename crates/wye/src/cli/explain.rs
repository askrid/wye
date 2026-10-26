//! `wye explain`: what `wye serve` would decide for one request, offline.
//!
//! The checks call the same functions the server uses (credential
//! verification, the policy scanner, the permission rule, session sampling)
//! in the server's order, and make no network calls. Routing is a pure
//! function of request, credential, and config, so the answer is what every
//! replica would do.

use gateway_core::{
    config::{Config, UpstreamCredential},
    credential::{Identity, Verifier},
    policy::{self, PolicyScanner},
    prompt,
    upgrade::{self, UpgradeScanner},
};

/// The gateway credential a request carries.
pub enum Credential<'a> {
    None,
    /// A bearer token, verified as the server would, with the server's verifier.
    Token(&'a str, &'a Verifier),
    /// Claims taken as verified (policy tests; tokens in CI would expire).
    Trusted(Identity),
}

/// The decision and the steps that led to it.
#[derive(Debug)]
pub struct Explanation {
    pub steps: Vec<String>,
    /// `Some((status, error type, message))` when the request is refused.
    pub refused: Option<(u16, &'static str, String)>,
    /// The model the request runs on, after alias resolution, when the body
    /// was read.
    pub model: Option<String>,
    /// `(target model, rule)` for each upgrade-check issue.
    pub upgrade_issues: Vec<(String, &'static str)>,
}

impl Explanation {
    fn step(&mut self, line: String) {
        self.steps.push(line);
    }

    fn refuse(mut self, status: u16, kind: &'static str, message: String) -> Self {
        self.steps
            .push(format!("decision: refuse {status} {kind}: {message}"));
        self.refused = Some((status, kind, message));
        self
    }

    pub fn report(&self) -> String {
        self.steps.join("\n") + "\n"
    }
}

/// Explains `POST path` with `body`, `credential`, and the Claude Code
/// `session` id, at Unix time `now`. Upgrade checks run against `targets`,
/// or, when empty, against `[upgrade_check] targets`.
pub fn explain(
    config: &Config,
    path: &str,
    credential: Credential<'_>,
    session: Option<&str>,
    body: &[u8],
    now: u64,
    targets: &[&str],
) -> Explanation {
    let mut out = Explanation {
        steps: Vec::new(),
        refused: None,
        model: None,
        upgrade_issues: Vec::new(),
    };
    if !matches!(path, "/v1/messages" | "/v1/messages/count_tokens") {
        return out.refuse(404, "not_found_error", format!("{path} is not proxied"));
    }
    out.step(format!("endpoint: POST {path}"));

    let mut identity = None;
    match &config.auth {
        None => out.step("credential: not required (no [auth])".into()),
        Some(_) => match credential {
            Credential::None => {
                return out.refuse(401, "authentication_error", "no gateway credential".into());
            }
            Credential::Token(token, verifier) => match verifier.verify(token, now) {
                Ok(verified) => {
                    out.step(format!(
                        "credential: valid; subject {}, key {}, expires at {}",
                        verified.subject, verified.key_id, verified.expires_at
                    ));
                    identity = Some(verified);
                }
                Err(e) => return out.refuse(401, "authentication_error", e.to_string()),
            },
            Credential::Trusted(claims) => {
                out.step(format!(
                    "credential: taken as verified (signature not checked); subject {}",
                    claims.subject
                ));
                identity = Some(claims);
            }
        },
    }

    let limit = config.server.max_request_body_bytes;
    if body.len() as u64 > limit {
        return out.refuse(
            413,
            "request_too_large",
            format!("{} bytes exceed max_request_body_bytes {limit}", body.len()),
        );
    }
    out.step(format!("body: {} bytes (limit {limit})", body.len()));

    let configured = config.auth.as_ref().and_then(|a| a.models.as_deref());
    let granted = identity.as_ref().and_then(|i| i.models.as_deref());
    let list = |patterns: Option<&[policy::ModelPattern]>| match patterns {
        None => "any".to_owned(),
        // An identity provider's token that matched no grant.
        Some([]) => "none".to_owned(),
        Some(p) => p
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    };
    // The server reads the body once, for every check, whenever one applies.
    let token_limit = (path == "/v1/messages")
        .then(|| {
            policy::token_limit(
                config.auth.as_ref().and_then(|a| a.max_output_tokens),
                identity.as_ref().and_then(|i| i.max_output_tokens),
            )
        })
        .flatten();
    if configured.is_none()
        && granted.is_none()
        && token_limit.is_none()
        && config.aliases.is_none()
    {
        out.step(
            "model aliases, permissions, and token ceiling: none apply; the body is not read"
                .into(),
        );
    } else {
        let mut scanner = PolicyScanner::new();
        scanner.feed(body);
        let mut requested = match scanner.finish() {
            Ok(m) => m.to_owned(),
            Err(e) => return out.refuse(400, "invalid_request_error", e.to_string()),
        };
        if let Some(alias) = config.aliases.as_ref().and_then(|a| a.get(&requested)) {
            let subject = identity.as_ref().map(|i| &*i.subject);
            let resolved = alias.model_for(subject);
            out.step(format!(
                "alias: {requested} resolves to {resolved}{}; the body names {resolved} upstream",
                subject.map_or(String::new(), |s| format!(" for subject {s}"))
            ));
            requested = resolved.to_string();
        }
        out.model = Some(requested.clone());
        out.step(format!(
            "model: {requested}; allowed by config: {}; by credential: {}",
            list(configured),
            list(granted)
        ));
        if !policy::permitted(configured, granted, &requested) {
            return out.refuse(
                403,
                "permission_error",
                format!("this credential may not use {requested}"),
            );
        }
        if let Some(limit) = token_limit {
            match scanner.max_tokens() {
                Some(requested) if requested > limit => {
                    return out.refuse(
                        403,
                        "permission_error",
                        format!(
                            "max_tokens {requested} exceeds this credential's limit of {limit}"
                        ),
                    );
                }
                Some(requested) => out.step(format!("max_tokens: {requested} (limit {limit})")),
                None => out.step(format!(
                    "max_tokens: absent (limit {limit}; the provider requires it)"
                )),
            }
        }
    }

    let upstream = &config.upstream;
    out.step(format!(
        "upstream: POST {}://{}{}{path}",
        upstream.scheme, upstream.authority, upstream.path_prefix
    ));
    out.step(match &upstream.credential {
        UpstreamCredential::ApiKeyEnv(name) => {
            format!("provider credential: x-api-key from ${name}")
        }
        UpstreamCredential::ApiKeyFile(path) => {
            format!("provider credential: x-api-key from {}", path.display())
        }
        UpstreamCredential::ForwardCaller => "provider credential: the caller's own".into(),
    });
    if let Some(diagnostics) = &config.cache_diagnostics {
        let sampled =
            prompt::session_sampled(session.map(str::as_bytes), diagnostics.sample_percent);
        out.step(format!(
            "cache diagnostics: {} (sample_percent {})",
            if sampled {
                "this request is hashed"
            } else {
                "this session is not sampled"
            },
            diagnostics.sample_percent
        ));
    }
    let configured_targets = config
        .upgrade_check
        .as_ref()
        .map_or(&[][..], |c| &c.targets[..]);
    let targets: Vec<&str> = if targets.is_empty() {
        configured_targets.iter().map(|t| &**t).collect()
    } else {
        targets.to_vec()
    };
    if !targets.is_empty() {
        let mut scanner = UpgradeScanner::new();
        scanner.feed(body);
        let findings = scanner.findings();
        for target in targets {
            let rules: Vec<upgrade::Rule> = upgrade::issues(&findings, target).collect();
            if rules.is_empty() {
                out.step(format!("upgrade check: {target} accepts this request"));
            }
            for rule in rules {
                out.step(format!(
                    "upgrade check: {target} would reject it: {} ({}; {})",
                    rule.name(),
                    rule.fix(),
                    rule.source()
                ));
                out.upgrade_issues.push((target.to_owned(), rule.name()));
            }
        }
    }
    out.step("decision: forward".into());
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use gateway_core::credential::{self, NewCredential};
    use ring::signature::KeyPair;

    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn config(key: &ring::signature::Ed25519KeyPair, extra_auth: &str) -> Config {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\nmax_request_body_bytes = 200\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n\
             [auth]\nissuer = \"wye\"\naudience = \"wye-prod\"\n{extra_auth}\n\
             [[auth.keys]]\nid = \"k1\"\ned25519_public_key = \"{}\"\n\
             [cache_diagnostics]\nhash_key_env = \"K\"",
            credential::encode_public_key(key.public_key().as_ref())
        ))
        .unwrap()
    }

    fn verifier(config: &Config) -> Verifier {
        Verifier::new(config.auth.as_ref().unwrap(), Vec::new()).unwrap()
    }

    fn token(key: &ring::signature::Ed25519KeyPair, models: Option<&[&str]>) -> String {
        credential::issue(
            key,
            "k1",
            &NewCredential {
                iss: "wye",
                sub: "alice",
                aud: "wye-prod",
                iat: NOW,
                exp: NOW + 600,
                models,
                max_output_tokens: None,
            },
        )
    }

    #[test]
    fn explains_forwarding_and_each_refusal() {
        let key = wye_testkit::ed25519_keypair();
        let config = config(&key, "models = [\"claude-*\"]");
        let haiku = br#"{"model":"claude-haiku-4-5","messages":[]}"#;
        let t = token(&key, Some(&["claude-haiku-*"]));

        let ok = explain(
            &config,
            "/v1/messages",
            Credential::Token(&t, &verifier(&config)),
            Some("s-1"),
            haiku,
            NOW,
            &[],
        );
        assert!(ok.refused.is_none(), "{}", ok.report());
        let report = ok.report();
        assert!(report.contains("subject alice"), "{report}");
        assert!(report.contains("by credential: claude-haiku-*"), "{report}");
        assert!(
            report.contains("upstream: POST https://api.anthropic.com/v1/messages"),
            "{report}"
        );
        assert!(report.ends_with("decision: forward\n"), "{report}");

        let sonnet = br#"{"model":"claude-sonnet-4-5"}"#;
        let refused = |e: Explanation| e.refused.map(|(status, kind, _)| (status, kind));
        assert_eq!(
            refused(explain(
                &config,
                "/v1/messages",
                Credential::Token(&t, &verifier(&config)),
                None,
                sonnet,
                NOW,
                &[]
            )),
            Some((403, "permission_error"))
        );
        assert_eq!(
            refused(explain(
                &config,
                "/v1/messages",
                Credential::None,
                None,
                haiku,
                NOW,
                &[]
            )),
            Some((401, "authentication_error"))
        );
        assert_eq!(
            refused(explain(
                &config,
                "/v1/messages",
                Credential::Token(&t, &verifier(&config)),
                None,
                haiku,
                NOW + 601,
                &[]
            )),
            Some((401, "authentication_error"))
        );
        assert_eq!(
            refused(explain(
                &config,
                "/v1/messages",
                Credential::Token(&t, &verifier(&config)),
                None,
                &[b' '; 201],
                NOW,
                &[]
            )),
            Some((413, "request_too_large"))
        );
        assert_eq!(
            refused(explain(
                &config,
                "/v1/messages",
                Credential::Token(&t, &verifier(&config)),
                None,
                b"{}",
                NOW,
                &[]
            )),
            Some((400, "invalid_request_error"))
        );
        assert_eq!(
            refused(explain(
                &config,
                "/v1/other",
                Credential::Token(&t, &verifier(&config)),
                None,
                haiku,
                NOW,
                &[]
            )),
            Some((404, "not_found_error"))
        );
        let mut limited = config.clone();
        limited.auth.as_mut().unwrap().max_output_tokens = Some(100);
        let over = br#"{"model":"claude-haiku-4-5","max_tokens":101}"#;
        assert_eq!(
            refused(explain(
                &limited,
                "/v1/messages",
                Credential::Token(&t, &verifier(&config)),
                None,
                over,
                NOW,
                &[]
            )),
            Some((403, "permission_error"))
        );
        let report = explain(
            &limited,
            "/v1/messages",
            Credential::Token(&t, &verifier(&config)),
            None,
            br#"{"model":"claude-haiku-4-5","max_tokens":100}"#,
            NOW,
            &[],
        )
        .report();
        assert!(report.contains("max_tokens: 100 (limit 100)"), "{report}");

        // A token ceiling alone still makes the server read the body, so a
        // request without a model is refused, as `serve` refuses it.
        let mut only_ceiling = limited.clone();
        only_ceiling.auth.as_mut().unwrap().models = None;
        let t = token(&key, None);
        assert_eq!(
            refused(explain(
                &only_ceiling,
                "/v1/messages",
                Credential::Token(&t, &verifier(&only_ceiling)),
                None,
                br#"{"max_tokens":10}"#,
                NOW,
                &[]
            )),
            Some((400, "invalid_request_error"))
        );
    }

    #[test]
    fn explains_alias_resolution_before_permissions() {
        let config = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
             [[aliases]]\nname = \"coding\"\nmodel = \"claude-opus-5-5\"\n",
        )
        .unwrap();
        let report = explain(
            &config,
            "/v1/messages",
            Credential::None,
            None,
            br#"{"model":"coding"}"#,
            NOW,
            &[],
        )
        .report();
        assert!(
            report.contains("alias: coding resolves to claude-opus-5-5;"),
            "{report}"
        );
        assert!(report.contains("model: claude-opus-5-5;"), "{report}");
        assert!(report.ends_with("decision: forward\n"), "{report}");
    }

    #[test]
    fn explains_upgrade_issues_per_target_without_refusing() {
        let config = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
             [upgrade_check]\ntargets = [\"claude-opus-4-7\"]\n",
        )
        .unwrap();
        let body = br#"{"model":"claude-opus-4-6","thinking":{"type":"enabled","budget_tokens":9},"messages":[{"role":"system","content":"x"}]}"#;
        let configured = explain(
            &config,
            "/v1/messages",
            Credential::None,
            None,
            body,
            NOW,
            &[],
        );
        assert!(configured.refused.is_none(), "report only");
        assert_eq!(
            configured.upgrade_issues,
            [
                ("claude-opus-4-7".to_owned(), "budget_tokens"),
                ("claude-opus-4-7".to_owned(), "system_role")
            ]
        );
        let named = explain(
            &config,
            "/v1/messages",
            Credential::None,
            None,
            body,
            NOW,
            &["claude-opus-4-6"],
        );
        assert!(named.upgrade_issues.is_empty());
        assert!(
            named
                .report()
                .contains("upgrade check: claude-opus-4-6 accepts this request")
        );
    }
}
