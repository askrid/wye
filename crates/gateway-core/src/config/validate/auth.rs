//! `[auth]`: Wye's own issuer and the identity providers.

use std::path::PathBuf;

use serde::Deserialize;

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAuth {
    pub(crate) issuer: Option<String>,
    pub(crate) audience: Option<String>,
    pub(crate) max_gateway_token_ttl_secs: Option<u64>,
    #[serde(default)]
    pub(crate) signing_keys: Vec<RawPublicKey>,
    #[serde(default)]
    pub(crate) identity_providers: Vec<RawIdentityProvider>,
    pub(crate) models: Option<Vec<String>>,
    pub(crate) max_output_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPublicKey {
    pub(crate) id: String,
    pub(crate) ed25519_public_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawIdentityProvider {
    pub(crate) issuer: String,
    pub(crate) audience: String,
    pub(crate) algorithm: String,
    pub(crate) jwks_file: PathBuf,
    pub(crate) subject_claim: Option<String>,
    #[serde(default)]
    pub(crate) grants: Vec<RawGrant>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawGrant {
    pub(crate) claim: String,
    pub(crate) value: String,
    pub(crate) models: Vec<String>,
}

pub(crate) fn validate_auth(raw: RawAuth) -> Result<Auth, ConfigError> {
    let err = |msg: String| ConfigError::Auth(msg);
    let own = match (raw.issuer, raw.audience, raw.signing_keys.is_empty()) {
        (None, None, true) => None,
        (Some(issuer), Some(audience), false) => {
            Some(validate_own(issuer, audience, raw.signing_keys)?)
        }
        (Some(_), Some(_), true) => {
            return Err(err(
                "issuer and audience need at least one [[auth.signing_keys]] entry".into(),
            ));
        }
        _ => {
            return Err(err(
                "issuer, audience, and [[auth.signing_keys]] go together; omit all three to \
                 trust only [[auth.identity_providers]]"
                    .into(),
            ));
        }
    };
    if own.is_none() && raw.identity_providers.is_empty() {
        return Err(err(
            "set issuer, audience, and [[auth.signing_keys]], or at least one \
             [[auth.identity_providers]]"
                .into(),
        ));
    }
    let mut identity_providers: Vec<IdentityProvider> =
        Vec::with_capacity(raw.identity_providers.len());
    for (i, raw_idp) in raw.identity_providers.into_iter().enumerate() {
        let idp = validate_identity_provider(raw_idp, i)?;
        let taken = own.as_ref().is_some_and(|o| o.issuer == idp.issuer)
            || identity_providers
                .iter()
                .any(|other| other.issuer == idp.issuer);
        if taken {
            return Err(err(format!("duplicate issuer {:?}", idp.issuer)));
        }
        identity_providers.push(idp);
    }
    let models = match raw.models {
        Some(list) if list.is_empty() => {
            return Err(err(
                "models must not be empty; omit it to allow every model".into(),
            ));
        }
        Some(list) => Some(patterns(&list, "models").map_err(err)?),
        None => None,
    };
    Ok(Auth {
        own,
        identity_providers,
        models,
        max_output_tokens: raw
            .max_output_tokens
            .map(|n| nonzero(n, "auth.max_output_tokens"))
            .transpose()?,
        max_gateway_token_ttl: token_ttl(
            raw.max_gateway_token_ttl_secs
                .unwrap_or(DEFAULT_MAX_GATEWAY_TOKEN_TTL_SECS),
        )?,
    })
}

/// `max_gateway_token_ttl_secs`: more than zero, at most
/// [`MAX_GATEWAY_TOKEN_TTL_SECS`].
fn token_ttl(secs: u64) -> Result<Duration, ConfigError> {
    const FIELD: &str = "auth.max_gateway_token_ttl_secs";
    if secs > MAX_GATEWAY_TOKEN_TTL_SECS {
        return Err(ConfigError::TooLong {
            field: FIELD,
            max: MAX_GATEWAY_TOKEN_TTL_SECS,
        });
    }
    nonzero(secs, FIELD).map(Duration::from_secs)
}

fn validate_own(
    issuer: String,
    audience: String,
    raw_keys: Vec<RawPublicKey>,
) -> Result<OwnIssuer, ConfigError> {
    let err = |msg: String| ConfigError::Auth(msg);
    if issuer.is_empty() || audience.is_empty() {
        return Err(err("issuer and audience must not be empty".into()));
    }
    let mut keys = Vec::with_capacity(raw_keys.len());
    for key in raw_keys {
        if key.id.is_empty() {
            return Err(err("key id must not be empty".into()));
        }
        if keys.iter().any(|k: &PublicKey| k.id == key.id) {
            return Err(err(format!("duplicate key id {:?}", key.id)));
        }
        let Some(public) = crate::token::decode_public_key(&key.ed25519_public_key) else {
            return Err(err(format!(
                "key {:?}: ed25519_public_key must be 32 bytes in unpadded base64url",
                key.id
            )));
        };
        keys.push(PublicKey {
            id: key.id,
            ed25519_public_key: public,
        });
    }
    Ok(OwnIssuer {
        issuer,
        audience,
        keys,
    })
}

fn validate_identity_provider(
    raw: RawIdentityProvider,
    index: usize,
) -> Result<IdentityProvider, ConfigError> {
    let err = |msg: String| ConfigError::Auth(format!("identity_providers[{index}]: {msg}"));
    if raw.issuer.is_empty() || raw.audience.is_empty() {
        return Err(err("issuer and audience must not be empty".into()));
    }
    let Some(algorithm) = Algorithm::parse(&raw.algorithm) else {
        return Err(err(format!(
            "algorithm {:?} is not RS256, ES256, or EdDSA",
            raw.algorithm
        )));
    };
    if raw.jwks_file.as_os_str().is_empty() {
        return Err(err("jwks_file must not be empty".into()));
    }
    let subject_claim = raw.subject_claim.unwrap_or_else(|| "sub".to_owned());
    if subject_claim.is_empty() {
        return Err(err("subject_claim must not be empty".into()));
    }
    let mut grants = Vec::with_capacity(raw.grants.len());
    for grant in raw.grants {
        if grant.claim.is_empty() || grant.value.is_empty() {
            return Err(err("grants need a non-empty claim and value".into()));
        }
        if grant.models.is_empty() {
            return Err(err(format!(
                "grant {}={:?}: models must not be empty",
                grant.claim, grant.value
            )));
        }
        grants.push(Grant {
            models: patterns(&grant.models, "grants.models").map_err(err)?,
            claim: grant.claim,
            value: grant.value,
        });
    }
    Ok(IdentityProvider {
        issuer: raw.issuer,
        audience: raw.audience,
        algorithm,
        jwks_file: raw.jwks_file,
        subject_claim,
        grants,
    })
}

fn patterns(list: &[String], field: &str) -> Result<Vec<ModelPattern>, String> {
    list.iter()
        .map(|p| {
            ModelPattern::parse(p).ok_or_else(|| {
                format!("{field}: {p:?} is not a model name or a prefix ending in *")
            })
        })
        .collect()
}
