//! Gateway credentials: compact JWTs, from Wye's own issuer (Ed25519) or from
//! configured identity providers (RS256, ES256, or EdDSA).
//!
//! A token's `kid` selects its key, and the key belongs to one issuer, whose
//! one algorithm the header must name: the algorithm is fixed by
//! configuration, never chosen by the token, so `alg: none` and
//! algorithm-confusion attacks have nothing to select. Claims are parsed only
//! after the signature verifies, and then must name that key's issuer.
//!
//! Wye's own tokens reject unknown header fields (including `crit`). Identity
//! provider tokens carry extra header fields (`typ`, `x5t`), which are
//! ignored, except `crit`, which is rejected (RFC 7515 §4.1.11). Unknown
//! claims are ignored.
//!
//! Verification is pure: the caller passes the current time.

use std::{collections::HashMap, fmt};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::signature::Ed25519KeyPair;
use serde::{Deserialize, Serialize, de::IgnoredAny};

use crate::{
    config::{Algorithm, Auth, Grant},
    policy::ModelPattern,
};

mod keys;
mod provider;

use keys::VerifyKey;
pub use keys::{KeySetError, ProviderKeys};

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
    /// Models this credential may use (the `models` claim, or an identity
    /// provider's matching grants); `None` leaves the configured list in
    /// charge. An empty list allows no model.
    pub models: Option<Box<[ModelPattern]>>,
    /// Largest output-token maximum (`max_tokens`, `max_output_tokens`,
    /// `max_completion_tokens`) this credential may request (the
    /// `max_output_tokens` claim).
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
    MissingSubject,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "credential too large",
            Self::Malformed => "credential is not a well-formed token",
            Self::UnsupportedAlgorithm => "credential algorithm is not the one its issuer uses",
            Self::UnknownKey => "credential signed by an unknown key",
            Self::BadSignature => "credential signature is invalid",
            Self::WrongIssuer => "credential issuer is not accepted",
            Self::WrongAudience => "credential audience does not match",
            Self::Expired => "credential has expired",
            Self::NotYetValid => "credential is not yet valid",
            Self::LifetimeTooLong => "credential lifetime exceeds the configured maximum",
            Self::MissingSubject => "credential lacks its subject claim",
        })
    }
}

impl std::error::Error for CredentialError {}

/// The header of Wye's own tokens: exactly these fields.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Header<'a> {
    alg: &'a str,
    kid: &'a str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    typ: Option<&'a str>,
}

/// Any token's header, read only to select the key.
#[derive(Deserialize)]
struct AnyHeader<'a> {
    alg: &'a str,
    kid: Option<&'a str>,
    crit: Option<IgnoredAny>,
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

/// The registered claims every token is checked on.
struct Registered<'a> {
    iss: &'a str,
    aud: &'a Audience,
    exp: u64,
    iat: u64,
    nbf: Option<u64>,
}

/// Who signs with a key, and what its tokens are checked for.
struct Source {
    issuer: Box<str>,
    audience: Box<str>,
    kind: SourceKind,
}

enum SourceKind {
    /// `wye token issue`: Ed25519, strict header, `models` and
    /// `max_output_tokens` claims.
    Own,
    /// An identity provider: its pinned algorithm, subject claim, and grants.
    Provider {
        algorithm: Algorithm,
        subject_claim: Box<str>,
        grants: Box<[Grant]>,
    },
}

/// Checks credentials against the configured issuers and keys.
pub struct Verifier {
    max_ttl_secs: u64,
    /// Every key, Wye's own and the providers', with the index of its source.
    keys: HashMap<Box<str>, (VerifyKey, usize)>,
    sources: Vec<Source>,
}

impl fmt::Debug for Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Verifier")
            .field(
                "issuers",
                &self.sources.iter().map(|s| &*s.issuer).collect::<Vec<_>>(),
            )
            .field("keys", &self.keys.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Verifier {
    /// `provider_keys` holds each `auth.issuers` entry's parsed key set, in
    /// order; an issuer without one accepts no token. A key id used twice,
    /// in any two sets, is an error: a token's `kid` must name one key.
    pub fn new(auth: &Auth, provider_keys: Vec<ProviderKeys>) -> Result<Self, KeySetError> {
        let mut verifier = Self {
            max_ttl_secs: auth.max_credential_ttl.as_secs(),
            keys: HashMap::new(),
            sources: Vec::new(),
        };
        if let Some(own) = &auth.own {
            let index = verifier.add_source(&own.issuer, &own.audience, SourceKind::Own);
            for key in &own.keys {
                verifier.add_key(&key.id, VerifyKey::ed25519(key.ed25519_public_key), index)?;
            }
        }
        for (issuer, set) in auth.issuers.iter().zip(provider_keys) {
            let kind = SourceKind::Provider {
                algorithm: issuer.algorithm,
                subject_claim: issuer.subject_claim.as_str().into(),
                grants: issuer.grants.clone().into(),
            };
            let index = verifier.add_source(&issuer.issuer, &issuer.audience, kind);
            for (kid, key) in set.keys {
                verifier.add_key(&kid, key, index)?;
            }
        }
        Ok(verifier)
    }

    fn add_source(&mut self, issuer: &str, audience: &str, kind: SourceKind) -> usize {
        self.sources.push(Source {
            issuer: issuer.into(),
            audience: audience.into(),
            kind,
        });
        self.sources.len() - 1
    }

    fn add_key(&mut self, kid: &str, key: VerifyKey, source: usize) -> Result<(), KeySetError> {
        if self.keys.insert(kid.into(), (key, source)).is_some() {
            return Err(KeySetError::DuplicateKid(kid.to_owned()));
        }
        Ok(())
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
        let header: AnyHeader<'_> =
            serde_json::from_slice(&header_json).map_err(|_| CredentialError::Malformed)?;
        let kid = header.kid.ok_or(CredentialError::UnknownKey)?;
        let (key, source) = self.keys.get(kid).ok_or(CredentialError::UnknownKey)?;
        let source = self
            .sources
            .get(*source)
            .ok_or(CredentialError::UnknownKey)?;
        let algorithm = match &source.kind {
            SourceKind::Own => {
                serde_json::from_slice::<Header<'_>>(&header_json)
                    .map_err(|_| CredentialError::Malformed)?;
                Algorithm::EdDsa.as_str()
            }
            SourceKind::Provider { algorithm, .. } => {
                if header.crit.is_some() {
                    return Err(CredentialError::Malformed);
                }
                algorithm.as_str()
            }
        };
        if header.alg != algorithm {
            return Err(CredentialError::UnsupportedAlgorithm);
        }
        // The signature covers the encoded header and claims exactly as sent.
        let signed_len = header_b64.len() + 1 + claims_b64.len();
        let signed = token.get(..signed_len).ok_or(CredentialError::Malformed)?;
        if !key.verify(signed.as_bytes(), &decode(signature_b64)?) {
            return Err(CredentialError::BadSignature);
        }

        // Claims are parsed only after the signature holds.
        let claims_json = decode(claims_b64)?;
        match &source.kind {
            SourceKind::Own => self.own_identity(source, &claims_json, kid, now),
            SourceKind::Provider {
                subject_claim,
                grants,
                ..
            } => self.provider_identity(source, &claims_json, subject_claim, grants, kid, now),
        }
    }

    fn own_identity(
        &self,
        source: &Source,
        claims_json: &[u8],
        kid: &str,
        now: u64,
    ) -> Result<Identity, CredentialError> {
        let claims: Claims =
            serde_json::from_slice(claims_json).map_err(|_| CredentialError::Malformed)?;
        self.check(
            source,
            &Registered {
                iss: &claims.iss,
                aud: &claims.aud,
                exp: claims.exp,
                iat: claims.iat,
                nbf: claims.nbf,
            },
            now,
        )?;
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
            key_id: kid.into(),
            expires_at: claims.exp,
        })
    }

    fn check(&self, source: &Source, r: &Registered<'_>, now: u64) -> Result<(), CredentialError> {
        if r.iss != &*source.issuer {
            return Err(CredentialError::WrongIssuer);
        }
        if !r.aud.contains(&source.audience) {
            return Err(CredentialError::WrongAudience);
        }
        if now >= r.exp {
            return Err(CredentialError::Expired);
        }
        let earliest = r.nbf.unwrap_or(r.iat).max(r.iat);
        if earliest > now.saturating_add(CLOCK_SKEW_SECS) {
            return Err(CredentialError::NotYetValid);
        }
        if r.exp.saturating_sub(r.iat) > self.max_ttl_secs {
            return Err(CredentialError::LifetimeTooLong);
        }
        Ok(())
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
    /// Largest output-token maximum; `None` omits the claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

/// Signs `claims` with `key` under key id `kid`.
pub fn issue(key: &Ed25519KeyPair, kid: &str, claims: &NewCredential<'_>) -> String {
    let header = Header {
        alg: Algorithm::EdDsa.as_str(),
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
pub(crate) fn decode_public_key(encoded: &str) -> Option<[u8; 32]> {
    B64.decode(encoded).ok()?.try_into().ok()
}

#[cfg(test)]
mod tests;
