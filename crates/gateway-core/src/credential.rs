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
        Ok(Identity {
            models,
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
mod tests {
    use std::time::Duration;

    use ring::signature::KeyPair;

    use super::*;
    use crate::config::PublicKey;

    const NOW: u64 = 1_800_000_000;

    use wye_testkit::ed25519_keypair as keypair;

    fn verifier(key: &Ed25519KeyPair) -> Verifier {
        Verifier::new(&Auth {
            models: None,
            issuer: "wye".into(),
            audience: "wye-prod".into(),
            max_credential_ttl: Duration::from_secs(86_400),
            keys: vec![PublicKey {
                id: "k1".into(),
                ed25519_public_key: key.public_key().as_ref().try_into().unwrap(),
            }],
        })
    }

    fn claims<'a>() -> NewCredential<'a> {
        NewCredential {
            iss: "wye",
            sub: "alice",
            aud: "wye-prod",
            iat: NOW,
            exp: NOW + 3_600,
            models: None,
        }
    }

    #[test]
    fn issued_credential_verifies() {
        let key = keypair();
        let token = issue(&key, "k1", &claims());
        let identity = verifier(&key).verify(&token, NOW + 10).unwrap();
        assert_eq!(&*identity.subject, "alice");
        assert_eq!(&*identity.key_id, "k1");
        assert_eq!(identity.expires_at, NOW + 3_600);
        assert_eq!(identity.models, None);
    }

    #[test]
    fn models_claim_becomes_patterns_and_bad_lists_fail_closed() {
        let key = keypair();
        let v = verifier(&key);
        let with_models = |models: &[&str]| {
            let mut c = claims();
            c.models = Some(models);
            v.verify(&issue(&key, "k1", &c), NOW + 10)
        };
        let identity = with_models(&["claude-haiku-*", "claude-sonnet-4-5"]).unwrap();
        let models = identity.models.unwrap();
        assert!(models[0].matches("claude-haiku-4-5"));
        assert!(!models[1].matches("claude-sonnet-4-5-20250929"));
        assert_eq!(with_models(&[]), Err(CredentialError::Malformed));
        assert_eq!(with_models(&["a*b"]), Err(CredentialError::Malformed));
    }

    #[test]
    fn rejects_by_time_issuer_audience_and_lifetime() {
        let key = keypair();
        let v = verifier(&key);
        let with = |f: fn(&mut NewCredential<'_>)| {
            let mut c = claims();
            f(&mut c);
            issue(&key, "k1", &c)
        };
        assert_eq!(
            v.verify(&issue(&key, "k1", &claims()), NOW + 3_600),
            Err(CredentialError::Expired)
        );
        assert_eq!(
            v.verify(&with(|c| c.iss = "other"), NOW),
            Err(CredentialError::WrongIssuer)
        );
        assert_eq!(
            v.verify(&with(|c| c.aud = "wye-dev"), NOW),
            Err(CredentialError::WrongAudience)
        );
        assert_eq!(
            v.verify(&with(|c| c.exp = NOW + 2 * 86_400), NOW),
            Err(CredentialError::LifetimeTooLong)
        );
        assert_eq!(
            v.verify(&with(|c| c.iat = NOW + 3_000), NOW),
            Err(CredentialError::NotYetValid)
        );
        // Within the clock-skew allowance.
        assert!(v.verify(&with(|c| c.iat = NOW + 30), NOW).is_ok());
    }

    #[test]
    fn rejects_other_keys_and_tampering() {
        let key = keypair();
        let v = verifier(&key);
        assert_eq!(
            v.verify(&issue(&keypair(), "k1", &claims()), NOW),
            Err(CredentialError::BadSignature)
        );
        assert_eq!(
            v.verify(&issue(&key, "k2", &claims()), NOW),
            Err(CredentialError::UnknownKey)
        );
        // Swap in claims with another subject, keeping the original signature.
        let token = issue(&key, "k1", &claims());
        let mut parts: Vec<&str> = token.split('.').collect();
        let mallory = issue(
            &key,
            "k1",
            &NewCredential {
                sub: "mallory",
                ..claims()
            },
        );
        let forged_claims = mallory.split('.').nth(1).unwrap().to_owned();
        parts[1] = &forged_claims;
        assert_eq!(
            v.verify(&parts.join("."), NOW),
            Err(CredentialError::BadSignature)
        );
    }

    #[test]
    fn rejects_unsupported_algorithms_and_headers() {
        let key = keypair();
        let v = verifier(&key);
        let token = issue(&key, "k1", &claims());
        let rest = token.split_once('.').unwrap().1;
        for header in [
            r#"{"alg":"none","kid":"k1"}"#,
            r#"{"alg":"HS256","kid":"k1"}"#,
            r#"{"alg":"EdDSA","kid":"k1","crit":["exp"]}"#,
        ] {
            let forged = format!("{}.{rest}", B64.encode(header));
            let err = v.verify(&forged, NOW).unwrap_err();
            assert!(
                matches!(
                    err,
                    CredentialError::UnsupportedAlgorithm | CredentialError::Malformed
                ),
                "{header}: {err:?}"
            );
        }
    }

    #[test]
    fn rejects_malformed_and_oversized_tokens() {
        let v = verifier(&keypair());
        for token in ["", "a", "a.b", "a.b.c.d", "!!.!!.!!", "e30.e30.e30"] {
            assert!(v.verify(token, NOW).is_err(), "{token:?}");
        }
        assert_eq!(
            v.verify(&"a".repeat(MAX_TOKEN_BYTES + 1), NOW),
            Err(CredentialError::TooLarge)
        );
    }

    #[test]
    fn audience_may_be_a_list() {
        let key = keypair();
        let header = B64.encode(r#"{"alg":"EdDSA","kid":"k1"}"#);
        let claims = B64.encode(format!(
            r#"{{"iss":"wye","sub":"ci","aud":["x","wye-prod"],"iat":{NOW},"exp":{}}}"#,
            NOW + 60
        ));
        let signed = format!("{header}.{claims}");
        let token = format!("{signed}.{}", B64.encode(key.sign(signed.as_bytes())));
        assert_eq!(&*verifier(&key).verify(&token, NOW).unwrap().subject, "ci");
    }

    #[test]
    fn public_key_round_trips() {
        let key = keypair();
        let encoded = encode_public_key(key.public_key().as_ref());
        assert_eq!(
            decode_public_key(&encoded).unwrap(),
            key.public_key().as_ref()
        );
        assert!(decode_public_key("short").is_none());
    }
}
