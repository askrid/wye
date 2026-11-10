//! Gateway tokens: compact JWTs, from Wye's own issuer (Ed25519, `iss`
//! `wye`, `kid` the key's JWK thumbprint) or from configured identity
//! providers (RS256, ES256, or EdDSA).
//!
//! A token's `kid` selects its key, and the key belongs to one issuer, whose
//! one algorithm the header must name: the algorithm is fixed by
//! configuration, never chosen by the token, so `alg: none` and
//! algorithm-confusion attacks have nothing to select. Claims are parsed only
//! after the signature verifies, and then must name that key's issuer.
//!
//! Wye's own tokens reject unknown header fields (including `crit`).
//! Identity-provider tokens carry extra header fields (`typ`, `x5t`), which are
//! ignored, except `crit`, which is rejected (RFC 7515 §4.1.11). Unknown
//! claims are ignored.
//!
//! Verification is pure: the caller passes the current time.

use std::{collections::HashMap, fmt};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use serde::{Deserialize, Serialize, de::IgnoredAny};

use crate::{
    config::{Algorithm, Auth, Grant, OWN_ISSUER},
    policy::ModelPattern,
};

mod identity_provider;
mod issue;
mod keys;

pub use issue::{NewToken, encode_public_key, issue, thumbprint};
use keys::VerifyKey;
pub use keys::{IssuerKeys, KeySetError};

/// Longer tokens are rejected before any decoding.
const MAX_TOKEN_BYTES: usize = 8 * 1024;
/// Tolerated clock skew for `iat` and `nbf`. Expiry is strict.
const CLOCK_SKEW_SECS: u64 = 60;

/// Who a verified token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The `sub` claim, or the identity provider's subject claim.
    pub subject: Box<str>,
    /// The `kid` of the key that signed it.
    pub key_id: Box<str>,
    /// The `exp` claim, in Unix seconds.
    pub expires_at: u64,
    /// Models this token may use (the `models` claim, or an identity
    /// provider's matching grants); `None` leaves the configured list in
    /// charge. An empty list allows no model.
    pub models: Option<Box<[ModelPattern]>>,
    /// Largest output-token maximum this token may request (the
    /// `max_output_tokens` claim).
    pub max_output_tokens: Option<u64>,
}

impl Identity {
    /// Whether the token has expired at Unix time `now`.
    pub fn expired(&self, now: u64) -> bool {
        is_expired(self.expires_at, now)
    }
}

/// A token expiring at `exp` is valid strictly before it.
fn is_expired(exp: u64, now: u64) -> bool {
    now >= exp
}

/// Why a gateway token was refused. Deliberately coarse in client responses;
/// the variants exist for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// Longer than 8 KiB.
    TooLarge,
    /// Not three base64url parts of JSON in the shapes a token takes.
    Malformed,
    /// A header `alg` other than the key's issuer's one algorithm.
    UnsupportedAlgorithm,
    /// No `kid`, or one that names no configured key.
    UnknownKey,
    /// The signature does not verify.
    BadSignature,
    /// `iss` is not the key's issuer.
    WrongIssuer,
    /// `aud` does not name the issuer's audience.
    WrongAudience,
    /// Past `exp`.
    Expired,
    /// `iat` or `nbf` is in the future, beyond the clock skew allowed.
    NotYetValid,
    /// `exp - iat` exceeds `max_gateway_token_ttl`.
    LifetimeTooLong,
    /// An identity-provider token whose subject claim is absent, empty, or
    /// not a string.
    MissingSubject,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "token too large",
            Self::Malformed => "token is not well formed",
            Self::UnsupportedAlgorithm => "token algorithm is not the one its issuer uses",
            Self::UnknownKey => "token signed by an unknown key",
            Self::BadSignature => "token signature is invalid",
            Self::WrongIssuer => "token issuer is not accepted",
            Self::WrongAudience => "token audience does not match",
            Self::Expired => "token has expired",
            Self::NotYetValid => "token is not yet valid",
            Self::LifetimeTooLong => "token lifetime exceeds the configured maximum",
            Self::MissingSubject => "token lacks its subject claim",
        })
    }
}

impl std::error::Error for TokenError {}

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
    IdentityProvider {
        algorithm: Algorithm,
        subject_claim: Box<str>,
        grants: Box<[Grant]>,
    },
}

/// Checks tokens against the configured issuers and keys.
pub struct Verifier {
    max_ttl_secs: u64,
    /// Every key, Wye's own and the identity providers', with the index of its
    /// source.
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
    /// `issuer_keys` holds each `auth.identity_providers` entry's parsed key
    /// set, in order; an issuer without one accepts no token. A key id used
    /// twice, in any two sets, is an error: a token's `kid` must name one key.
    pub fn new(auth: &Auth, issuer_keys: Vec<IssuerKeys>) -> Result<Self, KeySetError> {
        let mut verifier = Self {
            max_ttl_secs: auth.max_gateway_token_ttl.as_secs(),
            keys: HashMap::new(),
            sources: Vec::new(),
        };
        if let Some(own) = &auth.own {
            let index = verifier.add_source(OWN_ISSUER, &own.audience, SourceKind::Own);
            for key in &own.keys {
                verifier.add_key(
                    &key.thumbprint,
                    VerifyKey::ed25519(key.ed25519_public_key),
                    index,
                )?;
            }
        }
        for (issuer, set) in auth.identity_providers.iter().zip(issuer_keys) {
            let kind = SourceKind::IdentityProvider {
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
    pub fn verify(&self, token: &str, now: u64) -> Result<Identity, TokenError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(TokenError::TooLarge);
        }
        let mut parts = token.split('.');
        let (Some(header_b64), Some(claims_b64), Some(signature_b64), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenError::Malformed);
        };

        let header_json = decode(header_b64)?;
        let header: AnyHeader<'_> =
            serde_json::from_slice(&header_json).map_err(|_| TokenError::Malformed)?;
        let kid = header.kid.ok_or(TokenError::UnknownKey)?;
        let (key, source) = self.keys.get(kid).ok_or(TokenError::UnknownKey)?;
        let source = self.sources.get(*source).ok_or(TokenError::UnknownKey)?;
        let algorithm = match &source.kind {
            SourceKind::Own => {
                serde_json::from_slice::<Header<'_>>(&header_json)
                    .map_err(|_| TokenError::Malformed)?;
                Algorithm::EdDsa.as_str()
            }
            SourceKind::IdentityProvider { algorithm, .. } => {
                if header.crit.is_some() {
                    return Err(TokenError::Malformed);
                }
                algorithm.as_str()
            }
        };
        if header.alg != algorithm {
            return Err(TokenError::UnsupportedAlgorithm);
        }
        // The signature covers the encoded header and claims exactly as sent.
        let signed_len = header_b64.len() + 1 + claims_b64.len();
        let signed = token.get(..signed_len).ok_or(TokenError::Malformed)?;
        if !key.verify(signed.as_bytes(), &decode(signature_b64)?) {
            return Err(TokenError::BadSignature);
        }

        // Claims are parsed only after the signature holds.
        let claims_json = decode(claims_b64)?;
        match &source.kind {
            SourceKind::Own => self.own_identity(source, &claims_json, kid, now),
            SourceKind::IdentityProvider {
                subject_claim,
                grants,
                ..
            } => self.idp_identity(source, &claims_json, subject_claim, grants, kid, now),
        }
    }

    fn own_identity(
        &self,
        source: &Source,
        claims_json: &[u8],
        kid: &str,
        now: u64,
    ) -> Result<Identity, TokenError> {
        let claims: Claims =
            serde_json::from_slice(claims_json).map_err(|_| TokenError::Malformed)?;
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
            Some(list) if list.is_empty() => return Err(TokenError::Malformed),
            Some(list) => Some(
                list.iter()
                    .map(|p| ModelPattern::parse(p).ok_or(TokenError::Malformed))
                    .collect::<Result<Box<[_]>, _>>()?,
            ),
            None => None,
        };
        if claims.max_output_tokens == Some(0) {
            return Err(TokenError::Malformed);
        }
        Ok(Identity {
            models,
            max_output_tokens: claims.max_output_tokens,
            subject: claims.sub.into(),
            key_id: kid.into(),
            expires_at: claims.exp,
        })
    }

    fn check(&self, source: &Source, r: &Registered<'_>, now: u64) -> Result<(), TokenError> {
        if r.iss != &*source.issuer {
            return Err(TokenError::WrongIssuer);
        }
        if !r.aud.contains(&source.audience) {
            return Err(TokenError::WrongAudience);
        }
        if is_expired(r.exp, now) {
            return Err(TokenError::Expired);
        }
        let earliest = r.nbf.unwrap_or(r.iat).max(r.iat);
        if earliest > now.saturating_add(CLOCK_SKEW_SECS) {
            return Err(TokenError::NotYetValid);
        }
        if r.exp.saturating_sub(r.iat) > self.max_ttl_secs {
            return Err(TokenError::LifetimeTooLong);
        }
        Ok(())
    }
}

fn decode(part: &str) -> Result<Vec<u8>, TokenError> {
    B64.decode(part).map_err(|_| TokenError::Malformed)
}

/// Decodes a key of `[auth] signing_keys`.
pub(crate) fn decode_public_key(encoded: &str) -> Option<[u8; 32]> {
    B64.decode(encoded).ok()?.try_into().ok()
}

#[cfg(test)]
mod tests;
