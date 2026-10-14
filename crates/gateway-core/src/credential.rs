//! Gateway credentials: compact JWTs signed with Ed25519.
//!
//! The algorithm is fixed by configuration, never taken from the token: a
//! header that does not say `EdDSA` is rejected, so `alg: none` and
//! algorithm-confusion attacks have nothing to select. Unknown header fields
//! (including `crit`) are rejected; unknown claims are ignored.
//!
//! Verification is pure: the caller passes the current time.

use std::{collections::HashMap, fmt};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::signature::{ED25519, Ed25519KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use crate::{config::Auth, policy::ModelPattern};

/// Longer tokens are rejected before any decoding.
const MAX_TOKEN_BYTES: usize = 8 * 1024;
/// Tolerated clock skew for `iat` and `nbf`. Expiry is strict.
const CLOCK_SKEW_SECS: u64 = 60;

/// Who a verified credential belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub subject: Box<str>,
    pub key_id: Box<str>,
    pub expires_at: u64,
    /// Models this credential may use (`models` claim); `None` leaves the
    /// configured list in charge.
    pub models: Option<Box<[ModelPattern]>>,
    /// Largest `max_tokens` this credential may request
    /// (`max_output_tokens` claim).
    pub max_output_tokens: Option<u64>,
}

/// Why a credential was rejected. Deliberately coarse in client responses;
/// the variants exist for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    TooLarge,
    Malformed,
    UnsupportedAlgorithm,
    UnknownKey,
    BadSignature,
    WrongIssuer,
    WrongAudience,
    Expired,
    NotYetValid,
    LifetimeTooLong,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "credential too large",
            Self::Malformed => "credential is not a well-formed token",
            Self::UnsupportedAlgorithm => "credential algorithm must be EdDSA",
            Self::UnknownKey => "credential signed by an unknown key",
            Self::BadSignature => "credential signature is invalid",
            Self::WrongIssuer => "credential issuer is not accepted",
            Self::WrongAudience => "credential audience does not match",
            Self::Expired => "credential has expired",
            Self::NotYetValid => "credential is not yet valid",
            Self::LifetimeTooLong => "credential lifetime exceeds the configured maximum",
        })
    }
}

impl std::error::Error for CredentialError {}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Header<'a> {
    alg: &'a str,
    kid: &'a str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    typ: Option<&'a str>,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: u64,
    iat: u64,
    nbf: Option<u64>,
    models: Option<Vec<String>>,
    max_output_tokens: Option<u64>,
}

/// `aud` may be one string or a list (RFC 7519 §4.1.3).
#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(aud) => aud == expected,
            Self::Many(auds) => auds.iter().any(|aud| aud == expected),
        }
    }
}

/// Checks credentials against the configured issuer, audience, and keys.
pub struct Verifier {
    issuer: Box<str>,
    audience: Box<str>,
    max_ttl_secs: u64,
    keys: HashMap<Box<str>, UnparsedPublicKey<[u8; 32]>>,
}

impl Verifier {
    pub fn new(auth: &Auth) -> Self {
        Self {
            issuer: auth.issuer.as_str().into(),
            audience: auth.audience.as_str().into(),
            max_ttl_secs: auth.max_credential_ttl.as_secs(),
            keys: auth
                .keys
                .iter()
                .map(|key| {
                    (
                        key.id.as_str().into(),
                        UnparsedPublicKey::new(&ED25519, key.ed25519_public_key),
                    )
                })
                .collect(),
        }
    }

    /// Verifies `token` at Unix time `now`, in seconds.
    pub fn verify(&self, token: &str, now: u64) -> Result<Identity, CredentialError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(CredentialError::TooLarge);
        }
        let mut parts = token.split('.');
        let (Some(header_b64), Some(claims_b64), Some(signature_b64), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(CredentialError::Malformed);
        };

        let header_json = decode(header_b64)?;
        let header: Header<'_> =
            serde_json::from_slice(&header_json).map_err(|_| CredentialError::Malformed)?;
        if header.alg != "EdDSA" {
            return Err(CredentialError::UnsupportedAlgorithm);
        }
        let key = self
            .keys
            .get(header.kid)
            .ok_or(CredentialError::UnknownKey)?;
        // The signature covers the encoded header and claims exactly as sent.
        let signed_len = header_b64.len() + 1 + claims_b64.len();
        let signed = token.get(..signed_len).ok_or(CredentialError::Malformed)?;
        key.verify(signed.as_bytes(), &decode(signature_b64)?)
            .map_err(|_| CredentialError::BadSignature)?;

        // Claims are parsed only after the signature holds.
        let claims: Claims =
            serde_json::from_slice(&decode(claims_b64)?).map_err(|_| CredentialError::Malformed)?;
        if claims.iss != *self.issuer {
            return Err(CredentialError::WrongIssuer);
        }
        if !claims.aud.contains(&self.audience) {
            return Err(CredentialError::WrongAudience);
        }
        if now >= claims.exp {
            return Err(CredentialError::Expired);
        }
        let earliest = claims.nbf.unwrap_or(claims.iat).max(claims.iat);
        if earliest > now.saturating_add(CLOCK_SKEW_SECS) {
            return Err(CredentialError::NotYetValid);
        }
        if claims.exp.saturating_sub(claims.iat) > self.max_ttl_secs {
            return Err(CredentialError::LifetimeTooLong);
        }
        // An empty or invalid list is an issuer mistake: fail closed.
        let models = match claims.models {
            Some(list) if list.is_empty() => return Err(CredentialError::Malformed),
            Some(list) => Some(
                list.iter()
                    .map(|p| ModelPattern::parse(p).ok_or(CredentialError::Malformed))
                    .collect::<Result<Box<[_]>, _>>()?,
            ),
            None => None,
        };
        if claims.max_output_tokens == Some(0) {
            return Err(CredentialError::Malformed);
        }
        Ok(Identity {
            models,
            max_output_tokens: claims.max_output_tokens,
            subject: claims.sub.into(),
            key_id: header.kid.into(),
            expires_at: claims.exp,
        })
    }
}

fn decode(part: &str) -> Result<Vec<u8>, CredentialError> {
    B64.decode(part).map_err(|_| CredentialError::Malformed)
}

/// Claims of a credential to issue.
#[derive(Serialize)]
pub struct NewCredential<'a> {
    pub iss: &'a str,
    pub sub: &'a str,
    pub aud: &'a str,
    pub iat: u64,
    pub exp: u64,
    /// Allowed models (names or prefixes ending in `*`); `None` omits the claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<&'a [&'a str]>,
    /// Largest `max_tokens`; `None` omits the claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

/// Signs `claims` with `key` under key id `kid`.
pub fn issue(key: &Ed25519KeyPair, kid: &str, claims: &NewCredential<'_>) -> String {
    let header = Header {
        alg: "EdDSA",
        kid,
        typ: Some("JWT"),
    };
    // Serializing these plain structs cannot fail.
    let header = B64.encode(serde_json::to_vec(&header).unwrap_or_default());
    let claims = B64.encode(serde_json::to_vec(claims).unwrap_or_default());
    let signed = format!("{header}.{claims}");
    let signature = B64.encode(key.sign(signed.as_bytes()));
    format!("{signed}.{signature}")
}

/// Encodes an Ed25519 public key for `ed25519_public_key` in the config.
pub fn encode_public_key(key: &[u8]) -> String {
    B64.encode(key)
}

/// Decodes `ed25519_public_key` from the config.
pub fn decode_public_key(encoded: &str) -> Option<[u8; 32]> {
    B64.decode(encoded).ok()?.try_into().ok()
}

#[cfg(test)]
mod tests;
