//! `[auth]`: Wye's own credential issuer and the identity providers.

use std::path::PathBuf;

use serde::Deserialize;

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAuth {
    pub(crate) issuer: Option<String>,
    pub(crate) audience: Option<String>,
    pub(crate) max_credential_ttl_secs: Option<u64>,
    #[serde(default)]
    pub(crate) keys: Vec<RawPublicKey>,
    #[serde(default)]
    pub(crate) issuers: Vec<RawIssuer>,
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
pub(crate) struct RawIssuer {
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

/// Default `max_credential_ttl_secs`: one week.
const DEFAULT_MAX_CREDENTIAL_TTL_SECS: u64 = 7 * 24 * 3600;

pub(crate) fn validate_auth(raw: RawAuth) -> Result<Auth, ConfigError> {
    let err = |msg: String| ConfigError::Auth(msg);
    let own = match (raw.issuer, raw.audience, raw.keys.is_empty()) {
        (None, None, true) => None,
        (Some(issuer), Some(audience), false) => Some(validate_own(issuer, audience, raw.keys)?),
        (Some(_), Some(_), true) => return Err(err("at least one key is required".into())),
        _ => {
            return Err(err(
                "issuer, audience, and [[auth.keys]] go together; omit all three to trust only \
                 [[auth.issuers]]"
                    .into(),
            ));
        }
    };
    if own.is_none() && raw.issuers.is_empty() {
        return Err(err(
            "set issuer, audience, and [[auth.keys]], or at least one [[auth.issuers]]".into(),
        ));
    }
    let mut issuers: Vec<Issuer> = Vec::with_capacity(raw.issuers.len());
    for (i, issuer) in raw.issuers.into_iter().enumerate() {
        let issuer = validate_issuer(issuer, i)?;
        let taken = own.as_ref().is_some_and(|o| o.issuer == issuer.issuer)
            || issuers.iter().any(|other| other.issuer == issuer.issuer);
        if taken {
            return Err(err(format!("duplicate issuer {:?}", issuer.issuer)));
        }
        issuers.push(issuer);
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
        issuers,
        models,
        max_output_tokens: raw
            .max_output_tokens
            .map(|n| nonzero(n, "auth.max_output_tokens"))
            .transpose()?,
        max_credential_ttl: secs(
            raw.max_credential_ttl_secs
                .unwrap_or(DEFAULT_MAX_CREDENTIAL_TTL_SECS),
            "auth.max_credential_ttl_secs",
        )?,
    })
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
        let Some(public) = crate::credential::decode_public_key(&key.ed25519_public_key) else {
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

fn validate_issuer(raw: RawIssuer, index: usize) -> Result<Issuer, ConfigError> {
    let err = |msg: String| ConfigError::Auth(format!("issuers[{index}]: {msg}"));
    if raw.issuer.is_empty() || raw.audience.is_empty() {
        return Err(err("issuer and audience must not be empty".into()));
    }
    let algorithm = match raw.algorithm.as_str() {
        "RS256" => Algorithm::Rs256,
        "ES256" => Algorithm::Es256,
        "EdDSA" => Algorithm::EdDsa,
        other => {
            return Err(err(format!(
                "algorithm {other:?} is not RS256, ES256, or EdDSA"
            )));
        }
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
    Ok(Issuer {
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
