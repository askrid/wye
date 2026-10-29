//! Verification keys: Wye's own Ed25519 keys and identity providers' keys
//! from a JSON Web Key Set (RFC 7517), parsed once at startup.
//!
//! A provider's key set is read for the one algorithm its issuer is pinned
//! to. Keys of another type, curve, or `alg`, and keys marked for encryption,
//! are skipped, so a provider's full published set can be committed as is.

use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::signature::{
    self, ECDSA_P256_SHA256_FIXED, ED25519, RsaPublicKeyComponents, UnparsedPublicKey,
};
use serde::Deserialize;

use crate::config::Algorithm;

/// RS256 keys of 2048 to 8192 bits, the range ring verifies.
const RSA_MODULUS_BYTES: std::ops::RangeInclusive<usize> = 256..=1024;
/// Largest JWKS document read; real ones are a few KB.
const MAX_JWKS_BYTES: usize = 1024 * 1024;

/// One public key, of the type its algorithm needs.
pub(super) enum VerifyKey {
    Ed25519(UnparsedPublicKey<[u8; 32]>),
    /// Uncompressed SEC1 point: `0x04 || x || y`.
    Es256(UnparsedPublicKey<[u8; 65]>),
    Rs256(RsaPublicKeyComponents<Box<[u8]>>),
}

impl VerifyKey {
    /// Whether `other` is the same public key.
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ed25519(a), Self::Ed25519(b)) => a.as_ref() == b.as_ref(),
            (Self::Es256(a), Self::Es256(b)) => a.as_ref() == b.as_ref(),
            (Self::Rs256(a), Self::Rs256(b)) => a.n == b.n && a.e == b.e,
            _ => false,
        }
    }

    pub(super) fn ed25519(public: [u8; 32]) -> Self {
        Self::Ed25519(UnparsedPublicKey::new(&ED25519, public))
    }

    /// Whether `signature` is valid for `message`. A JOSE ES256 signature is
    /// the fixed-width `r || s`, as ring's `FIXED` variant expects.
    pub(super) fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        match self {
            Self::Ed25519(key) => key.verify(message, signature).is_ok(),
            Self::Es256(key) => key.verify(message, signature).is_ok(),
            Self::Rs256(key) => key
                .verify(&signature::RSA_PKCS1_2048_8192_SHA256, message, signature)
                .is_ok(),
        }
    }
}

/// An identity provider's keys, by key id, for its pinned algorithm.
pub struct ProviderKeys {
    pub(super) keys: Vec<(Box<str>, VerifyKey)>,
}

impl fmt::Debug for ProviderKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.keys.iter().map(|(kid, _)| kid))
            .finish()
    }
}

/// Why a key set cannot be used. Startup fails on any of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySetError {
    TooLarge,
    /// Not a JSON Web Key Set.
    Json(String),
    /// A usable key without a `kid`: tokens select keys by `kid`.
    MissingKid,
    /// The same `kid` twice, within one set or across issuers.
    DuplicateKid(String),
    InvalidKey {
        kid: String,
        reason: &'static str,
    },
    /// No key in the set fits the issuer's algorithm.
    NoUsableKey(Algorithm),
}

impl fmt::Display for KeySetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "key set exceeds {MAX_JWKS_BYTES} bytes"),
            Self::Json(e) => write!(f, "not a JSON Web Key Set: {e}"),
            Self::MissingKid => f.write_str("a key has no kid"),
            Self::DuplicateKid(kid) => write!(f, "key id {kid:?} appears more than once"),
            Self::InvalidKey { kid, reason } => write!(f, "key {kid:?}: {reason}"),
            Self::NoUsableKey(alg) => write!(f, "no {} signing key in the set", alg.as_str()),
        }
    }
}

impl std::error::Error for KeySetError {}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

/// The members a key needs; others (`x5c`, `key_ops`, ...) are ignored.
#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    crv: Option<String>,
    n: Option<String>,
    e: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

impl ProviderKeys {
    /// Parses a JWKS document and keeps the signing keys for `algorithm`.
    pub fn parse(jwks: &[u8], algorithm: Algorithm) -> Result<Self, KeySetError> {
        if jwks.len() > MAX_JWKS_BYTES {
            return Err(KeySetError::TooLarge);
        }
        let set: Jwks =
            serde_json::from_slice(jwks).map_err(|e| KeySetError::Json(e.to_string()))?;
        let (kty, crv) = match algorithm {
            Algorithm::Rs256 => ("RSA", None),
            Algorithm::Es256 => ("EC", Some("P-256")),
            Algorithm::EdDsa => ("OKP", Some("Ed25519")),
        };
        let mut keys: Vec<(Box<str>, VerifyKey)> = Vec::new();
        for jwk in set.keys {
            let fits = jwk.kty == kty
                && jwk.crv.as_deref() == crv
                && jwk.usage.as_deref().is_none_or(|u| u == "sig")
                && jwk.alg.as_deref().is_none_or(|a| a == algorithm.as_str());
            if !fits {
                continue;
            }
            let kid = jwk.kid.clone().ok_or(KeySetError::MissingKid)?;
            if keys.iter().any(|(k, _)| **k == *kid) {
                return Err(KeySetError::DuplicateKid(kid));
            }
            let key = key(&jwk, algorithm).map_err(|reason| KeySetError::InvalidKey {
                kid: kid.clone(),
                reason,
            })?;
            keys.push((kid.into(), key));
        }
        if keys.is_empty() {
            return Err(KeySetError::NoUsableKey(algorithm));
        }
        Ok(Self { keys })
    }

    /// Key ids in the set, in document order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|(kid, _)| &**kid)
    }

    /// Ids of the keys this set trusts and `old` did not: new ids, and ids
    /// that now name another key.
    pub fn added_since<'a>(&'a self, old: &'a Self) -> impl Iterator<Item = &'a str> {
        self.keys
            .iter()
            .filter(|(kid, key)| !old.keys.iter().any(|(k, o)| k == kid && o.same(key)))
            .map(|(kid, _)| &**kid)
    }
}

fn key(jwk: &Jwk, algorithm: Algorithm) -> Result<VerifyKey, &'static str> {
    let member = |value: &Option<String>, name: &'static str| {
        let value = value.as_deref().ok_or(name)?;
        B64.decode(value)
            .map_err(|_| "a member is not unpadded base64url")
    };
    match algorithm {
        Algorithm::Rs256 => {
            let n = strip_leading_zeros(&member(&jwk.n, "missing n")?);
            let e = strip_leading_zeros(&member(&jwk.e, "missing e")?);
            if !RSA_MODULUS_BYTES.contains(&n.len()) {
                return Err("RSA modulus must be 2048 to 8192 bits");
            }
            if e.is_empty() || e.len() > 4 {
                return Err("RSA exponent out of range");
            }
            Ok(VerifyKey::Rs256(RsaPublicKeyComponents { n, e }))
        }
        Algorithm::Es256 => {
            let x: [u8; 32] = member(&jwk.x, "missing x")?
                .try_into()
                .map_err(|_| "x must be 32 bytes")?;
            let y: [u8; 32] = member(&jwk.y, "missing y")?
                .try_into()
                .map_err(|_| "y must be 32 bytes")?;
            // The leading 0x04 marks an uncompressed point.
            let mut point = [4u8; 65];
            for (dst, src) in point.iter_mut().skip(1).zip(x.iter().chain(&y)) {
                *dst = *src;
            }
            Ok(VerifyKey::Es256(UnparsedPublicKey::new(
                &ECDSA_P256_SHA256_FIXED,
                point,
            )))
        }
        Algorithm::EdDsa => {
            let x: [u8; 32] = member(&jwk.x, "missing x")?
                .try_into()
                .map_err(|_| "x must be 32 bytes")?;
            Ok(VerifyKey::ed25519(x))
        }
    }
}

/// ring reads `n` and `e` as minimal big-endian integers.
fn strip_leading_zeros(bytes: &[u8]) -> Box<[u8]> {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    bytes.get(start..).unwrap_or_default().into()
}
