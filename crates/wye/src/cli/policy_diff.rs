//! `wye policy-diff`: what a config change does to access, for review and
//! CI. It compares two configs offline and lists what each change widens,
//! narrows, or only changes; a change that widens access (a model newly
//! allowed, a raised ceiling, a new trusted signer or key, authentication
//! removed, a listener beyond loopback or on every interface, a new API
//! route, the operator's key where callers brought their own) makes the
//! command exit non-zero, so it can require a second review.
//!
//! Model patterns compare by coverage: `claude-*` covers `claude-opus-*`
//! and `claude-opus-5-5`, so narrowing a list never reads as widening.

use std::path::Path;

use gateway_core::{
    config::{Auth, Config, Issuer, UpstreamCredential},
    policy::ModelPattern,
    route::Route,
};

use crate::{StartupError, startup::key_set};

/// The comparison, line by line.
#[derive(Debug, Default)]
pub struct PolicyDiff {
    widens: Vec<String>,
    narrows: Vec<String>,
    changes: Vec<String>,
}

impl PolicyDiff {
    /// Whether any change widens access.
    pub fn widens(&self) -> bool {
        !self.widens.is_empty()
    }

    pub fn report(&self) -> String {
        let mut out = String::new();
        for (kind, lines) in [
            ("widens", &self.widens),
            ("narrows", &self.narrows),
            ("changes", &self.changes),
        ] {
            for line in lines {
                out.push_str(&format!("{kind}: {line}\n"));
            }
        }
        out.push_str(&match self.widens.len() {
            0 => "result: no access widened\n".to_owned(),
            n => format!("result: access widened ({n})\n"),
        });
        out
    }

    /// Adds what the identity providers' key sets change, each read from
    /// its config's `jwks_file` relative to that config's directory: a key
    /// newly trusted widens.
    pub fn key_sets(
        &mut self,
        old: &Config,
        old_dir: &Path,
        new: &Config,
        new_dir: &Path,
    ) -> Result<(), StartupError> {
        let (Some(old), Some(new)) = (&old.auth, &new.auth) else {
            return Ok(());
        };
        for issuer in &new.issuers {
            // A new identity provider is reported with its config.
            let Some(before) = old.issuers.iter().find(|i| i.issuer == issuer.issuer) else {
                continue;
            };
            let (was, now) = (key_set(before, old_dir)?, key_set(issuer, new_dir)?);
            let name = &issuer.issuer;
            for kid in now.added_since(&was) {
                self.widens
                    .push(format!("[[auth.issuers]] {name}: key {kid} newly trusted"));
            }
            for kid in was.added_since(&now) {
                self.narrows.push(format!(
                    "[[auth.issuers]] {name}: key {kid} no longer trusted"
                ));
            }
        }
        Ok(())
    }
}

/// Compares `old` with `new`; [`PolicyDiff::key_sets`] adds the key sets.
pub fn policy_diff(old: &Config, new: &Config) -> PolicyDiff {
    let mut diff = PolicyDiff::default();
    let (from, to) = (old.server.listen, new.server.listen);
    if from.ip().is_loopback() && !to.ip().is_loopback() {
        diff.widens.push(format!(
            "server.listen: {from} -> {to}, reachable beyond this machine"
        ));
    } else if to.ip().is_unspecified() && !from.ip().is_unspecified() {
        diff.widens
            .push(format!("server.listen: {from} -> {to}, on every interface"));
    } else if from != to {
        diff.changes.push(format!("server.listen: {from} -> {to}"));
    }
    match (&old.auth, &new.auth) {
        (Some(_), None) => diff
            .widens
            .push("[auth] removed: no credential is required".into()),
        (None, Some(_)) => diff
            .narrows
            .push("[auth] added: a credential is required".into()),
        (Some(old), Some(new)) => auth(&mut diff, old, new),
        (None, None) => {}
    }
    for (name, old, new) in [
        ("claude", &old.routes.claude, &new.routes.claude),
        ("openai", &old.routes.openai, &new.routes.openai),
    ] {
        route(&mut diff, name, old.as_ref(), new.as_ref());
    }
    diff
}

fn auth(diff: &mut PolicyDiff, old: &Auth, new: &Auth) {
    models(
        diff,
        "[auth] models",
        old.models.as_deref(),
        new.models.as_deref(),
    );
    match (old.max_output_tokens, new.max_output_tokens) {
        (Some(a), None) => diff
            .widens
            .push(format!("[auth] max_output_tokens: {a} -> no ceiling")),
        (Some(a), Some(b)) if b > a => diff
            .widens
            .push(format!("[auth] max_output_tokens: {a} -> {b}")),
        (Some(a), Some(b)) if b < a => diff
            .narrows
            .push(format!("[auth] max_output_tokens: {a} -> {b}")),
        (None, Some(b)) => diff
            .narrows
            .push(format!("[auth] max_output_tokens: no ceiling -> {b}")),
        _ => {}
    }
    let (a, b) = (
        old.max_credential_ttl.as_secs(),
        new.max_credential_ttl.as_secs(),
    );
    if b > a {
        diff.widens
            .push(format!("[auth] max_credential_ttl_secs: {a} -> {b}"));
    } else if b < a {
        diff.narrows
            .push(format!("[auth] max_credential_ttl_secs: {a} -> {b}"));
    }
    match (&old.own, &new.own) {
        (None, Some(_)) => diff
            .widens
            .push("[auth] Wye's own credentials accepted".into()),
        (Some(_), None) => diff
            .narrows
            .push("[auth] Wye's own credentials no longer accepted".into()),
        (Some(old), Some(new)) => {
            if (&old.issuer, &old.audience) != (&new.issuer, &new.audience) {
                diff.widens.push(format!(
                    "[auth] issuer and audience: {} {} -> {} {}",
                    old.issuer, old.audience, new.issuer, new.audience
                ));
            }
            for key in &new.keys {
                if !old
                    .keys
                    .iter()
                    .any(|k| k.ed25519_public_key == key.ed25519_public_key)
                {
                    diff.widens.push(format!(
                        "[[auth.keys]] {}: a new signing key is trusted",
                        key.id
                    ));
                }
            }
            for key in &old.keys {
                if !new
                    .keys
                    .iter()
                    .any(|k| k.ed25519_public_key == key.ed25519_public_key)
                {
                    diff.narrows
                        .push(format!("[[auth.keys]] {}: no longer trusted", key.id));
                }
            }
        }
        (None, None) => {}
    }
    for issuer in &new.issuers {
        match old.issuers.iter().find(|i| i.issuer == issuer.issuer) {
            None => diff.widens.push(format!(
                "[[auth.issuers]] {}: tokens from this identity provider accepted",
                issuer.issuer
            )),
            Some(before) => issuer_change(diff, before, issuer),
        }
    }
    for issuer in &old.issuers {
        if !new.issuers.iter().any(|i| i.issuer == issuer.issuer) {
            diff.narrows.push(format!(
                "[[auth.issuers]] {}: no longer accepted",
                issuer.issuer
            ));
        }
    }
}

/// The same identity provider in both configs.
fn issuer_change(diff: &mut PolicyDiff, old: &Issuer, new: &Issuer) {
    let name = &new.issuer;
    // Which tokens verify, and whose subject they carry: review each change.
    // The key set's keys compare in `PolicyDiff::key_sets`.
    if old.audience != new.audience
        || old.algorithm != new.algorithm
        || old.subject_claim != new.subject_claim
    {
        diff.widens.push(format!(
            "[[auth.issuers]] {name}: audience, algorithm, or subject claim changed"
        ));
    }
    match (old.grants.is_empty(), new.grants.is_empty()) {
        // No grants: every token gets the [auth] models ceiling.
        (false, true) => diff.widens.push(format!(
            "[[auth.issuers]] {name}: grants removed, every token gets the [auth] models ceiling"
        )),
        (true, false) => diff.narrows.push(format!(
            "[[auth.issuers]] {name}: grants added, a token matching none may use no model"
        )),
        (true, true) => {}
        (false, false) => {
            let granted = |issuer: &Issuer, claim: &str, value: &str| -> Vec<ModelPattern> {
                issuer
                    .grants
                    .iter()
                    .filter(|g| g.claim == claim && g.value == value)
                    .flat_map(|g| g.models.iter().cloned())
                    .collect()
            };
            let mut seen: Vec<(&str, &str)> = Vec::new();
            for grant in old.grants.iter().chain(&new.grants) {
                let key = (grant.claim.as_str(), grant.value.as_str());
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                let (before, after) = (granted(old, key.0, key.1), granted(new, key.0, key.1));
                models(
                    diff,
                    &format!("[[auth.issuers]] {name} grant {}={}", key.0, key.1),
                    Some(&before),
                    Some(&after),
                );
            }
        }
    }
}

/// A model allowlist: `None` allows every model.
fn models(
    diff: &mut PolicyDiff,
    what: &str,
    old: Option<&[ModelPattern]>,
    new: Option<&[ModelPattern]>,
) {
    match (old, new) {
        (Some(_), None) => diff.widens.push(format!("{what}: every model allowed")),
        (None, Some(new)) => diff
            .narrows
            .push(format!("{what}: limited to {}", list(new))),
        (Some(old), Some(new)) => {
            for pattern in new {
                if !old.iter().any(|o| o.covers(pattern)) {
                    diff.widens.push(format!("{what}: {pattern} newly allowed"));
                }
            }
            for pattern in old {
                if !new.iter().any(|n| n.covers(pattern)) {
                    diff.narrows
                        .push(format!("{what}: {pattern} no longer allowed"));
                }
            }
        }
        (None, None) => {}
    }
}

/// Where requests go. A route for an API that had none widens, and so does
/// an upstream that starts sending the operator's key where callers
/// brought their own; other changes are listed for review, since where
/// requests go is not who may send them.
fn route(diff: &mut PolicyDiff, name: &str, old: Option<&Route>, new: Option<&Route>) {
    match (old, new) {
        (None, Some(_)) => diff.widens.push(format!(
            "routes.{name} added: its API now reaches a provider"
        )),
        (Some(_), None) => diff.narrows.push(format!(
            "routes.{name} removed: its API is no longer served"
        )),
        _ => {}
    }
    // Where each upstream is, for the report: `[upstream]` has no id.
    let upstreams = |route: Option<&Route>| -> Vec<(String, String, UpstreamCredential)> {
        route
            .map(|r| {
                r.upstreams()
                    .iter()
                    .map(|u| {
                        (
                            u.id.as_deref().map_or_else(
                                || "[upstream]".to_owned(),
                                |id| format!("routes.{name}: upstream {id}"),
                            ),
                            format!("{}://{}{}", u.scheme, u.authority, u.path_prefix),
                            u.credential.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let (before, after) = (upstreams(old), upstreams(new));
    for (at, url, credential) in &after {
        let Some((_, was, was_credential)) = before.iter().find(|(i, ..)| i == at) else {
            diff.changes.push(format!("{at} added ({url})"));
            continue;
        };
        if was != url {
            diff.changes.push(format!("{at}: {was} -> {url}"));
        }
        match (was_credential, credential) {
            (UpstreamCredential::ForwardCaller, UpstreamCredential::ForwardCaller) => {}
            (UpstreamCredential::ForwardCaller, _) => diff.widens.push(format!(
                "{at} sends the operator's key, where callers brought their own"
            )),
            (_, UpstreamCredential::ForwardCaller) => diff
                .narrows
                .push(format!("{at} forwards each caller's own credential")),
            (a, b) if a != b => {
                diff.changes
                    .push(format!("{at} key: {} -> {}", key_source(a), key_source(b)))
            }
            _ => {}
        }
    }
    for (at, ..) in &before {
        if !after.iter().any(|(i, ..)| i == at) {
            diff.changes.push(format!("{at} removed"));
        }
    }
}

/// Where an upstream's key comes from, by name.
fn key_source(credential: &UpstreamCredential) -> String {
    match credential {
        UpstreamCredential::ApiKeyEnv(name) => format!("${name}"),
        UpstreamCredential::ApiKeyFile(path) => path.display().to_string(),
        UpstreamCredential::ForwardCaller => "the caller's own".to_owned(),
    }
}

fn list(patterns: &[ModelPattern]) -> String {
    patterns
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests;
