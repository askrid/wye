//! `wye policy-diff`: what a config change does to access, for review and
//! CI. It compares two configs offline and lists what each change widens,
//! narrows, or only changes; a change that widens access makes the command
//! exit non-zero, so it can require a second review.
//!
//! Model patterns compare by coverage: `claude-*` covers `claude-opus-*`
//! and `claude-opus-5-5`, so narrowing a list never reads as widening.

use std::path::Path;

use gateway_core::{
    config::{Auth, Config, IdentityProvider, OwnIssuer, ProviderKey, Upstream},
    duration::show,
    policy::ModelPattern,
    route::RouteId,
};

use super::{
    model_list,
    paint::{NARROWS, NOT_WIDENED, WIDENED, WIDENS},
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
            (WIDENS, &self.widens),
            (NARROWS, &self.narrows),
            ("changes:", &self.changes),
        ] {
            for line in lines {
                out.push_str(&format!("{kind} {line}\n"));
            }
        }
        out.push_str(&match self.widens.len() {
            0 => format!("{NOT_WIDENED}\n"),
            n => format!("{WIDENED} ({n})\n"),
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
        for issuer in &new.identity_providers {
            // A new identity provider is reported with its config.
            let Some(before) = old
                .identity_providers
                .iter()
                .find(|i| i.issuer == issuer.issuer)
            else {
                continue;
            };
            let (was, now) = (key_set(before, old_dir)?, key_set(issuer, new_dir)?);
            let name = &issuer.issuer;
            for kid in now.added_since(&was) {
                self.widens.push(format!(
                    "[[auth.identity_providers]] {name}: key {kid} newly trusted"
                ));
            }
            for kid in was.added_since(&now) {
                self.narrows.push(format!(
                    "[[auth.identity_providers]] {name}: key {kid} no longer trusted"
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
            .push("[auth] removed: no gateway token is required".into()),
        (None, Some(_)) => diff
            .narrows
            .push("[auth] added: a gateway token is required".into()),
        (Some(old), Some(new)) => auth(&mut diff, old, new),
        (None, None) => {}
    }
    for id in RouteId::ALL {
        match (old.routes.route(id), new.routes.route(id)) {
            (None, Some(_)) => diff.widens.push(format!(
                "the {} route added: its requests now reach a provider",
                id.label()
            )),
            (Some(_), None) => diff.narrows.push(format!(
                "the {} route removed: its requests are no longer served",
                id.label()
            )),
            _ => {}
        }
    }
    upstreams(&mut diff, old, new);
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
    let (a, b) = (old.max_gateway_token_ttl, new.max_gateway_token_ttl);
    let ttl = || format!("[auth] max_gateway_token_ttl: {} -> {}", show(a), show(b));
    if b > a {
        diff.widens.push(ttl());
    } else if b < a {
        diff.narrows.push(ttl());
    }
    match (&old.own, &new.own) {
        (None, Some(_)) => diff.widens.push("[auth] Wye tokens accepted".into()),
        (Some(_), None) => diff
            .narrows
            .push("[auth] Wye tokens no longer accepted".into()),
        (Some(old), Some(new)) => {
            if old.audience != new.audience {
                diff.widens.push(format!(
                    "[auth] audience: {} -> {}",
                    old.audience, new.audience
                ));
            }
            // Keys compare by thumbprint, which names a key in tokens and
            // in `wye explain`.
            let trusts = |issuer: &OwnIssuer, thumbprint: &str| {
                issuer.keys.iter().any(|k| k.thumbprint == thumbprint)
            };
            for key in &new.keys {
                if !trusts(old, &key.thumbprint) {
                    diff.widens.push(format!(
                        "[auth] signing_keys {}: a new signing key is trusted",
                        key.thumbprint
                    ));
                }
            }
            for key in &old.keys {
                if !trusts(new, &key.thumbprint) {
                    diff.narrows.push(format!(
                        "[auth] signing_keys {}: no longer trusted",
                        key.thumbprint
                    ));
                }
            }
        }
        (None, None) => {}
    }
    for issuer in &new.identity_providers {
        match old
            .identity_providers
            .iter()
            .find(|i| i.issuer == issuer.issuer)
        {
            None => diff.widens.push(format!(
                "[[auth.identity_providers]] {}: tokens from this identity provider accepted",
                issuer.issuer
            )),
            Some(before) => issuer_change(diff, before, issuer),
        }
    }
    for issuer in &old.identity_providers {
        if !new
            .identity_providers
            .iter()
            .any(|i| i.issuer == issuer.issuer)
        {
            diff.narrows.push(format!(
                "[[auth.identity_providers]] {}: no longer accepted",
                issuer.issuer
            ));
        }
    }
}

/// The same identity provider in both configs.
fn issuer_change(diff: &mut PolicyDiff, old: &IdentityProvider, new: &IdentityProvider) {
    let name = &new.issuer;
    // Which tokens verify, and whose subject they carry: review each change.
    // The key set's keys compare in `PolicyDiff::key_sets`.
    if old.audience != new.audience
        || old.algorithm != new.algorithm
        || old.subject_claim != new.subject_claim
    {
        diff.widens.push(format!(
            "[[auth.identity_providers]] {name}: audience, algorithm, or subject claim changed"
        ));
    }
    match (old.grants.is_empty(), new.grants.is_empty()) {
        // No grants: every token gets `[auth] models`.
        (false, true) => diff.widens.push(format!(
            "[[auth.identity_providers]] {name}: grants removed, every gateway token may \
             use the models in [auth] models"
        )),
        (true, false) => diff.narrows.push(format!(
            "[[auth.identity_providers]] {name}: grants added, a token matching none \
             may use no model"
        )),
        (true, true) => {}
        (false, false) => {
            let granted = |idp: &IdentityProvider, claim: &str, value: &str| -> Vec<ModelPattern> {
                idp.grants
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
                    &format!(
                        "[[auth.identity_providers]] {name} grant {}={}",
                        key.0, key.1
                    ),
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
            .push(format!("{what}: limited to {}", model_list(new))),
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

/// Where requests go. A route for an API that had none widens (above), and
/// so does an upstream that starts sending a provider key where callers
/// brought their own; other changes are listed for review, since where
/// requests go is not who may send them. Ids are unique across all
/// upstreams, so an upstream keeps its id when its provider moves it to
/// another route.
fn upstreams(diff: &mut PolicyDiff, old: &Config, new: &Config) {
    let before: Vec<&Upstream> = old.routes.upstreams().collect();
    for u in new.routes.upstreams() {
        let at = format!("upstreams {:?}", u.id);
        let (url, credential) = (u.base_url(), &u.provider_key);
        let Some(was) = before.iter().find(|w| w.id == u.id) else {
            diff.changes.push(format!("{at} added ({url})"));
            continue;
        };
        if was.provider != u.provider {
            let (from, to) = (was.provider.route(), u.provider.route());
            let moved = if from == to {
                String::new()
            } else {
                format!(
                    ", from the {} route to the {} route",
                    from.label(),
                    to.label()
                )
            };
            diff.changes.push(format!(
                "{at}: provider {} -> {}{moved}",
                was.provider.as_str(),
                u.provider.as_str()
            ));
        }
        if was.base_url() != url {
            diff.changes
                .push(format!("{at}: {} -> {url}", was.base_url()));
        }
        match (was.standby, u.standby) {
            (false, true) => diff.changes.push(format!("{at}: now standby")),
            (true, false) => diff.changes.push(format!("{at}: no longer standby")),
            _ => {}
        }
        match (&was.provider_key, credential) {
            (ProviderKey::ForwardCaller, ProviderKey::ForwardCaller) => {}
            (ProviderKey::ForwardCaller, _) => diff.widens.push(format!(
                "{at} sends a provider key, where callers brought their own"
            )),
            (_, ProviderKey::ForwardCaller) => diff
                .narrows
                .push(format!("{at} forwards each caller's own credential")),
            (a, b) if a != b => diff.changes.push(format!("{at} provider key: {a} -> {b}")),
            _ => {}
        }
    }
    for was in before {
        if !new.routes.upstreams().any(|u| u.id == was.id) {
            diff.changes.push(format!("upstreams {:?} removed", was.id));
        }
    }
}

#[cfg(test)]
mod tests;
