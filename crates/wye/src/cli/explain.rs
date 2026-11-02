//! `wye explain`: what `wye serve` would decide for one request, offline.
//!
//! The checks call the same functions the server uses (credential
//! verification, the policy scanner, body admission's [`BodyChecks`], session
//! sampling) in the server's order, and make no network calls. Routing is a
//! pure function of request, credential, and config, so the answer is what
//! every replica would do.

use super::{args::Parsed, files};
use gateway_core::{
    admission::{self, BodyChecks, Refusal},
    api::{self, Api, Endpoint},
    config::{Config, Upstream, UpstreamCredential},
    credential::{Identity, Verifier},
    policy,
    pricing::{Pico, PricedModels, Usd},
    prompt,
    route::{self, FailoverReason, Route, Select},
    upgrade::{self, UpgradeScanner},
};

use super::{
    model_list,
    paint::{FORWARD, REFUSE},
    size,
};

/// The gateway token a request carries.
pub enum Credential<'a> {
    None,
    /// A bearer token, verified as the server would, with the server's verifier.
    Token(&'a str, &'a Verifier),
    /// Claims taken as verified (policy tests; tokens in CI would expire).
    Trusted(Identity),
}

/// `wye explain`, from its options: the report, with the config it read, and
/// whether the request would be forwarded.
pub fn run(
    parsed: &Parsed<'_>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(String, bool), String> {
    let path = files::config_path(parsed, env)?;
    let config = files::load(&path)?;
    let verifier = files::named(
        &path,
        crate::startup::verifier(&config, files::config_dir(&path)),
    )?;
    let token = parsed
        .value("token-file")
        .map(files::read_token)
        .transpose()?;
    let body = match parsed.positional.first() {
        Some(&"-") => {
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut body)
                .map_err(|e| format!("cannot read standard input: {e}"))?;
            body
        }
        Some(file) => std::fs::read(file).map_err(|e| format!("cannot read {file}: {e}"))?,
        None => Vec::new(),
    };
    let credential = match (token.as_deref(), &verifier) {
        (Some(token), Some(verifier)) => Credential::Token(token, verifier),
        _ => Credential::None,
    };
    let explanation = explain(
        &config,
        parsed.value("path").unwrap_or("/v1/messages"),
        credential,
        parsed.value("session"),
        &body,
        super::unix_secs()?,
        &parsed.values("target-model"),
    );
    let forwarded = explanation.refused.is_none();
    Ok((
        format!(
            "config: {}\n{}",
            files::tilde(&path, env),
            explanation.report()
        ),
        forwarded,
    ))
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
    /// The route upstream's id, with a route of named upstreams.
    pub upstream: Option<String>,
}

/// A refusal ends the walk; its answer is already in the [`Explanation`].
struct Refused;

/// One stage of the walk: `Err` once the request is refused.
type Stage<T = ()> = Result<T, Refused>;

impl Explanation {
    fn step(&mut self, line: String) {
        self.steps.push(line);
    }

    fn refuse(&mut self, status: u16, kind: &'static str, message: String) -> Refused {
        self.steps
            .push(format!("{REFUSE} {status} {kind}: {message}"));
        self.refused = Some((status, kind, message));
        Refused
    }

    /// The server's status and type for `refusal`, with explain's own, more
    /// specific message.
    fn refuse_as(&mut self, refusal: &Refusal, message: String) -> Refused {
        self.refuse(refusal.status(), refusal.kind(), message)
    }

    /// A policy refusal, worded as the server words it.
    fn refuse_policy(&mut self, refusal: &Refusal) -> Refused {
        self.refuse(refusal.status(), refusal.kind(), refusal.to_string())
    }

    pub fn report(&self) -> String {
        self.steps.join("\n") + "\n"
    }

    /// The endpoint `path` names and the route serving it.
    fn endpoint<'c>(&mut self, config: &'c Config, path: &str) -> Stage<(Endpoint, &'c Route)> {
        let Some(endpoint) = api::endpoint(path) else {
            return Err(self.refuse_as(&Refusal::NotProxied, format!("{path} is not proxied")));
        };
        let Some(route) = config.routes.get(endpoint.api) else {
            return Err(self.refuse_policy(&Refusal::NoRoute {
                api: endpoint.api,
                path: path.into(),
            }));
        };
        self.step(format!("endpoint: POST {path}"));
        Ok((endpoint, route))
    }

    /// The caller's identity, verified as the server verifies it.
    fn token(
        &mut self,
        config: &Config,
        credential: Credential<'_>,
        now: u64,
    ) -> Stage<Option<Identity>> {
        if config.auth.is_none() {
            self.step("token: not required (no [auth])".into());
            return Ok(None);
        }
        match credential {
            Credential::None => {
                Err(self.refuse_as(&Refusal::Unauthenticated, "no gateway token".into()))
            }
            Credential::Token(token, verifier) => match verifier.verify(token, now) {
                Ok(verified) => {
                    self.step(format!(
                        "token: valid; subject {}, key {}, expires at {}",
                        verified.subject, verified.key_id, verified.expires_at
                    ));
                    Ok(Some(verified))
                }
                Err(e) => Err(self.refuse_as(&Refusal::Unauthenticated, e.to_string())),
            },
            Credential::Trusted(claims) => {
                self.step(format!(
                    "token: taken as verified (signature not checked); subject {}",
                    claims.subject
                ));
                Ok(Some(claims))
            }
        }
    }

    /// The body's size against `max_request_body_bytes`.
    fn body_size(&mut self, config: &Config, body: &[u8]) -> Stage {
        let limit = config.server.max_request_body_bytes;
        if body.len() as u64 > limit {
            return Err(self.refuse_as(
                &Refusal::TooLarge,
                format!("{} bytes exceed max_request_body_bytes {limit}", body.len()),
            ));
        }
        self.step(format!(
            "body: {} bytes (limit {})",
            body.len(),
            size(limit)
        ));
        Ok(())
    }

    /// Aliases, model permissions, and output-token ceilings, read from the
    /// body as the server's hold reads it.
    fn policy(
        &mut self,
        config: &Config,
        api: Api,
        checks: &BodyChecks<'_>,
        identity: Option<&Identity>,
        body: &[u8],
    ) -> Stage {
        let configured = config.auth.as_ref().and_then(|a| a.models.as_deref());
        let list = |patterns: Option<&[policy::ModelPattern]>| match patterns {
            None => "any".to_owned(),
            // An identity provider's token that matched no grant.
            Some([]) => "none".to_owned(),
            Some(p) => model_list(p),
        };
        if !checks.hold() {
            self.step(
                "model aliases, permissions, and output-token ceiling: none apply; the body is not read"
                    .into(),
            );
            return Ok(());
        }
        // The server holds the body until its model is read, decides on what
        // it knows then, and checks the rest as it streams; explain reads the
        // file the same way, as if the hold ended right after the model.
        let mut scanner = admission::scanner(api, checks.token_limit);
        let held = held_len(api, checks.token_limit, body);
        let (start, rest) = body.split_at(held);
        scanner.feed(start);
        let sent = match scanner.finish() {
            Ok(m) => m.to_owned(),
            Err(e) => return Err(self.refuse_policy(&Refusal::Invalid(e))),
        };
        let subject = identity.map(|i| &*i.subject);
        let model = match checks.resolve(&sent, subject) {
            Some((_, resolved)) => {
                self.step(format!(
                    "alias: {sent} resolves to {resolved}{}; the body names {resolved} upstream",
                    subject.map_or(String::new(), |s| format!(" for subject {s}"))
                ));
                resolved.to_string()
            }
            None => sent.clone(),
        };
        self.model = Some(model.clone());
        self.step(format!(
            "model: {model}; allowed by config: {}; by token: {}",
            list(configured),
            list(checks.granted)
        ));
        if let Err(refusal) = checks.permit(&model, &sent) {
            return Err(self.refuse_policy(&refusal));
        }
        if let Err(refusal) = checks.check_tokens(api, scanner.max_output_tokens(), rest.is_empty())
        {
            return Err(self.refuse_policy(&refusal));
        }
        // A refusal in the rest aborts the upstream request mid-body, with
        // the same answer.
        if checks.checks_rest() {
            scanner.feed(rest);
            if let Some(e) = scanner.error() {
                return Err(self.refuse_policy(&Refusal::Invalid(e)));
            }
        }
        if let Some(limit) = checks.token_limit {
            let requested = scanner.max_output_tokens();
            if let Err(refusal) = checks.check_tokens(api, requested, true) {
                return Err(self.refuse_policy(&refusal));
            }
            let field = api.token_field();
            self.step(match requested {
                Some(requested) => format!("{field}: {requested} (limit {limit})"),
                None => format!("{field}: absent (limit {limit}; the provider requires it)"),
            });
        }
        Ok(())
    }

    /// The upstream the route picks for this request, and the order
    /// failover would follow.
    fn route<'r>(
        &mut self,
        route: &'r Route,
        session: Option<&str>,
        identity: Option<&Identity>,
    ) -> &'r Upstream {
        let ranking = route.rank(
            route::affinity_key(session.map(str::as_bytes), identity.map(|i| &*i.subject))
                .unwrap_or_default(),
        );
        let upstreams = route.upstreams();
        let Some(upstream) = upstreams.get(ranking.first()) else {
            unreachable!("a route has at least one upstream")
        };
        if let Some(id) = &upstream.id {
            self.upstream = Some(id.to_string());
            let how = match (route.select(), session, identity) {
                (Select::Ordered, _, _) => "ordered: the first in config",
                (Select::CacheAffine, Some(_), _) => "cache_affine by session id",
                (Select::CacheAffine, None, Some(_)) => "cache_affine by token subject",
                (Select::CacheAffine, None, None) => {
                    "cache_affine by nothing: no session id or token"
                }
            };
            self.step(format!(
                "route: upstream {id} of {} ({how})",
                upstreams.len()
            ));
        }
        if let Some(failover) = route.failover() {
            let next: Vec<&str> = ranking
                .iter()
                .skip(1)
                .filter_map(|i| upstreams.get(i)?.id.as_deref())
                .collect();
            let reasons: Vec<&str> = failover.reasons().map(FailoverReason::as_str).collect();
            self.step(format!(
                "failover: on {} to {} in that order, for bodies up to {}",
                reasons.join(", "),
                next.join(", "),
                size(failover.max_body_bytes)
            ));
        }
        upstream
    }

    /// Where the request goes and the key it carries.
    fn upstream(&mut self, upstream: &Upstream, path: &str) {
        self.step(format!("upstream: POST {}{path}", upstream.base_url()));
        if let Some(workspace) = &upstream.workspace_id {
            self.step(format!(
                "workspace: anthropic-workspace-id {workspace} (Claude Platform on AWS)"
            ));
        }
        let header = upstream.provider.key_header();
        self.step(match &upstream.credential {
            UpstreamCredential::ForwardCaller => "provider key: the caller's own".into(),
            source => format!("provider key: {header} from {source}"),
        });
    }

    /// The provider's own model id and the price, for the model the body
    /// names.
    fn model_and_price(
        &mut self,
        config: &Config,
        upstream: &Upstream,
        api: Api,
        token_limit: Option<u64>,
        body: &[u8],
    ) {
        // The gateway reads the model on a route that rewrites its id even
        // when no policy does; explain reads it for the price too.
        let model = self.model.clone().or_else(|| {
            let mut scanner = admission::scanner(api, token_limit);
            scanner.feed(body);
            scanner.finish().ok().map(str::to_owned)
        });
        if let Some(model) = &model
            && let Some(prefix) = upstream.provider.prefix_for(model)
        {
            self.step(format!(
                "model id: the body names {prefix}{model} upstream, the provider's id for {model}"
            ));
        }
        if let (Some(pricing), Some(model)) = (&config.pricing, &model) {
            let at = upstream
                .id
                .as_deref()
                .map_or(String::new(), |id| format!(" at {id}"));
            self.step(match pricing.entry(model, upstream.id.as_deref()) {
                Some(entry) => format!(
                    "price: {model}{at}: {} (pricing version {})",
                    prices(entry),
                    pricing.version
                ),
                None => format!("price: none for {model}{at}; its cost is recorded as unknown"),
            });
        }
    }

    /// Cache diagnostics and upgrade checks, which read Anthropic request
    /// bodies. Upgrade checks run against `targets`, else `[upgrade_check]`'s.
    fn body_scans(
        &mut self,
        config: &Config,
        session: Option<&str>,
        body: &[u8],
        targets: &[&str],
    ) {
        if let Some(diagnostics) = &config.cache_diagnostics {
            let sampled =
                prompt::session_sampled(session.map(str::as_bytes), diagnostics.sample_percent);
            self.step(format!(
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
        if targets.is_empty() {
            return;
        }
        let mut scanner = UpgradeScanner::new();
        scanner.feed(body);
        let findings = scanner.findings();
        for target in targets {
            let rules: Vec<upgrade::Rule> = upgrade::issues(&findings, target).collect();
            if rules.is_empty() {
                self.step(format!("upgrade check: {target} accepts this request"));
            }
            for rule in rules {
                self.step(format!(
                    "upgrade check: {target} would reject it: {} ({}; {})",
                    rule.name(),
                    rule.fix(),
                    rule.source()
                ));
            }
        }
    }
}

/// Explains `POST path` with `body`, `credential`, and the client's
/// `session` id (Claude Code's or Codex's), at Unix time `now`. Upgrade
/// checks run against `targets`, or, when empty, against
/// `[upgrade_check] targets`.
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
        upstream: None,
    };
    // The server's checks, in its order. Each stage records its steps; a
    // refusal ends the walk with its answer already recorded.
    let _ = (|| -> Stage {
        // The server matches the path; the query string is forwarded.
        let path = path.split_once('?').map_or(path, |(path, _)| path);
        let (endpoint, route) = out.endpoint(config, path)?;
        let api = endpoint.api;
        // Chat Completions carries no session header: the server never sees one.
        let session = session.filter(|_| api.session_header().is_some());
        let identity = out.token(config, credential, now)?;
        out.body_size(config, body)?;
        let checks = BodyChecks::new(
            config.auth.as_ref().and_then(|a| a.models.as_deref()),
            config.auth.as_ref().and_then(|a| a.max_output_tokens),
            config.aliases.as_ref(),
            identity.as_ref(),
            endpoint.generates,
            route.prefixes_model_ids(),
        );
        out.policy(config, api, &checks, identity.as_ref(), body)?;
        let upstream = out.route(route, session, identity.as_ref());
        out.upstream(upstream, path);
        out.model_and_price(config, upstream, api, checks.token_limit, body);
        if api == Api::Messages {
            out.body_scans(config, session, body, targets);
        }
        out.step(FORWARD.into());
        Ok(())
    })();
    out
}

/// Bytes of `body` through the end of its top-level `model` value, where
/// the server's hold ends; the whole body when there is none to find.
fn held_len(api: Api, token_limit: Option<u64>, body: &[u8]) -> usize {
    let mut probe = admission::scanner(api, token_limit);
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
