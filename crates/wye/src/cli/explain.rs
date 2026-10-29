//! `wye explain`: what `wye serve` would decide for one request, offline.
//!
//! The checks call the same functions the server uses (credential
//! verification, the policy scanner, the permission rule, session sampling)
//! in the server's order, and make no network calls. Routing is a pure
//! function of request, credential, and config, so the answer is what every
//! replica would do.

use gateway_core::{
    api::{self, Api},
    config::{Config, UpstreamCredential},
    credential::{Identity, Verifier},
    policy::{self, PolicyScanner},
    pricing::{Pico, PricedModels, Usd},
    prompt,
    route::Select,
    upgrade::{self, UpgradeScanner},
};

use super::size;
use crate::request_body::TokenLimitError;

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
    /// The route upstream's id, with `[routes.claude]`.
    pub upstream: Option<String>,
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
        upstream: None,
    };
    // The server matches the path; the query string is forwarded.
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    let Some(endpoint) = api::endpoint(path) else {
        return out.refuse(404, "not_found_error", format!("{path} is not proxied"));
    };
    let Some(route) = config.routes.get(endpoint.api) else {
        return out.refuse(
            404,
            "not_found_error",
            format!(
                "no route serves {path}: add {}",
                endpoint.api.route_sections()
            ),
        );
    };
    let api = endpoint.api;
    out.step(format!("endpoint: POST {path}"));
    // Chat Completions carries no session header: the server never sees one.
    let session = session.filter(|_| api.session_header().is_some());

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
    out.step(format!(
        "body: {} bytes (limit {})",
        body.len(),
        size(limit)
    ));

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
    let token_limit = endpoint
        .generates
        .then(|| {
            policy::token_limit(
                config.auth.as_ref().and_then(|a| a.max_output_tokens),
                identity.as_ref().and_then(|i| i.max_output_tokens),
            )
        })
        .flatten();
    // A provider with its own model ids on the route: the model is read.
    let prefixes_model_ids = config.routes.get(api).is_some_and(|r| {
        r.upstreams()
            .iter()
            .any(|u| u.provider.model_prefix().is_some())
    });
    if configured.is_none()
        && granted.is_none()
        && token_limit.is_none()
        && config.aliases.is_none()
        && !prefixes_model_ids
    {
        out.step(
            "model aliases, permissions, and output-token ceiling: none apply; the body is not read"
                .into(),
        );
    } else {
        // The server holds the body until its model is read, decides on what
        // it knows then, and checks the rest as it streams; explain reads the
        // file the same way, as if the hold ended right after the model.
        let mut scanner = PolicyScanner::for_api(api);
        let held = held_len(api, body);
        let (start, rest) = body.split_at(held);
        scanner.feed(start);
        let mut requested = match scanner.finish() {
            Ok(m) => m.to_owned(),
            Err(e) => return out.refuse(400, "invalid_request_error", e.to_string()),
        };
        // The server's refusal names the model as the client sent it.
        let sent = requested.clone();
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
                format!("this credential may not use the model {sent:?}"),
            );
        }
        // A value is final at the hold; a missing field only at the end.
        if let Some(limit) = token_limit
            && (scanner.max_output_tokens().is_some() || rest.is_empty())
            && let Some(refusal) = TokenLimitError::check(api, limit, scanner.max_output_tokens())
        {
            return out.refuse(403, "permission_error", refusal.to_string());
        }
        // The rest is checked only for permissions and ceilings, not for an
        // alias or a provider's model id alone. A refusal there aborts the
        // upstream request mid-body, with the same answer.
        if configured.is_some() || granted.is_some() || token_limit.is_some() {
            scanner.feed(rest);
            if let Some(e) = scanner.error() {
                return out.refuse(400, "invalid_request_error", e.to_string());
            }
        }
        if let Some(limit) = token_limit {
            let requested = scanner.max_output_tokens();
            if let Some(refusal) = TokenLimitError::check(api, limit, requested) {
                return out.refuse(403, "permission_error", refusal.to_string());
            }
            let field = api.token_field();
            out.step(match requested {
                Some(requested) => format!("{field}: {requested} (limit {limit})"),
                None => format!("{field}: absent (limit {limit}; the provider requires it)"),
            });
        }
    }

    // The server's affinity key: the session id, else the credential subject.
    let key = session
        .map(str::as_bytes)
        .or(identity.as_ref().map(|i| i.subject.as_bytes()))
        .unwrap_or_default();
    let upstreams = route.upstreams();
    let Some(upstream) = upstreams.get(route.rank(key).first()) else {
        unreachable!("a route has at least one upstream")
    };
    if let Some(id) = &upstream.id {
        out.upstream = Some(id.to_string());
        let how = match (route.select(), session, &identity) {
            (Select::Ordered, _, _) => "ordered: the first in config",
            (Select::CacheAffine, Some(_), _) => "cache_affine by session id",
            (Select::CacheAffine, None, Some(_)) => "cache_affine by credential subject",
            (Select::CacheAffine, None, None) => {
                "cache_affine by nothing: no session id or credential"
            }
        };
        out.step(format!(
            "route: upstream {id} of {} ({how})",
            upstreams.len()
        ));
    }
    if let Some(failover) = route.failover() {
        let next: Vec<&str> = route
            .rank(key)
            .iter()
            .skip(1)
            .filter_map(|i| upstreams.get(i)?.id.as_deref())
            .collect();
        let reasons: Vec<&str> = [
            (failover.on_429, "429"),
            (failover.on_529, "529"),
            (failover.on_connect, "connect"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        out.step(format!(
            "failover: on {} to {} in that order, for bodies up to {}",
            reasons.join(", "),
            next.join(", "),
            size(failover.max_body_bytes)
        ));
    }
    out.step(format!(
        "upstream: POST {}://{}{}{path}",
        upstream.scheme, upstream.authority, upstream.path_prefix
    ));
    let header = upstream.provider.key_header();
    if let Some(workspace) = &upstream.workspace_id {
        out.step(format!(
            "workspace: anthropic-workspace-id {workspace} (Claude Platform on AWS)"
        ));
    }
    out.step(match &upstream.credential {
        UpstreamCredential::ApiKeyEnv(name) => {
            format!("provider credential: {header} from ${name}")
        }
        UpstreamCredential::ApiKeyFile(path) => {
            format!("provider credential: {header} from {}", path.display())
        }
        UpstreamCredential::ForwardCaller => "provider credential: the caller's own".into(),
    });
    // The gateway reads the model on a route that rewrites its id even when no
    // policy does; explain reads it for the price too.
    let model = out.model.clone().or_else(|| {
        let mut scanner = PolicyScanner::for_api(api);
        scanner.feed(body);
        scanner.finish().ok().map(str::to_owned)
    });
    if let (Some(prefix), Some(model)) = (upstream.provider.model_prefix(), &model)
        && !model.starts_with(prefix)
    {
        out.step(format!(
            "model id: the body names {prefix}{model} upstream, the provider's id for {model}"
        ));
    }
    if let (Some(pricing), Some(model)) = (&config.pricing, &model) {
        let at = upstream
            .id
            .as_deref()
            .map_or(String::new(), |id| format!(" at {id}"));
        out.step(match pricing.entry(model, upstream.id.as_deref()) {
            Some(entry) => format!(
                "price: {model}{at}: {} (pricing version {})",
                prices(entry),
                pricing.version
            ),
            None => format!("price: none for {model}{at}; its cost is recorded as unknown"),
        });
    }
    // Prompt hashes and upgrade checks read Anthropic request bodies.
    if api != Api::Messages {
        out.step("decision: forward".into());
        return out;
    }
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

/// Bytes of `body` through the end of its top-level `model` value, where
/// the server's hold ends; the whole body when there is none to find.
fn held_len(api: Api, body: &[u8]) -> usize {
    let mut probe = PolicyScanner::for_api(api);
    probe.feed(body);
    probe
        .model_span()
        .and_then(|span| usize::try_from(span.end + 1).ok())
        .filter(|&end| end <= body.len())
        .unwrap_or(body.len())
}

#[cfg(test)]
mod tests;

/// An entry's prices per million tokens, exact and with at least two
/// decimals, as `[pricing]` writes them.
fn prices(entry: &PricedModels) -> String {
    let per_mtok = |p: Pico| {
        let usd = Usd(p.saturating_mul(1_000_000)).to_string();
        match usd.split_once('.').map(|(_, decimals)| decimals.len()) {
            None => format!("{usd}.00"),
            Some(1) => format!("{usd}0"),
            Some(_) => usd,
        }
    };
    let p = &entry.prices;
    let mut text = format!(
        "input ${}, output ${}",
        per_mtok(p.input),
        per_mtok(p.output)
    );
    for (name, price) in [
        ("cache read", p.cache_read),
        ("cache write 5m", p.cache_write_5m),
        ("cache write 1h", p.cache_write_1h),
    ] {
        if let Some(price) = price {
            text.push_str(&format!(", {name} ${}", per_mtok(price)));
        }
    }
    text.push_str(" per million tokens");
    if let Some(first) = entry.tiers.first() {
        text.push_str(&format!(
            "; {}, the first above {} prompt tokens",
            super::plural(entry.tiers.len() as u64, "tier", "tiers"),
            first.above_prompt_tokens
        ));
    }
    text
}
