//! Issuing Wye's own tokens, offline (`wye token issue`): the gateway only
//! verifies them.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::{
    digest::{SHA256, digest},
    signature::{Ed25519KeyPair, KeyPair},
};
use serde::Serialize;

use super::Header;
use crate::config::{Algorithm, OWN_ISSUER};

/// Claims of a token to issue; `iss` is always [`OWN_ISSUER`].
pub struct NewToken<'a> {
    pub sub: &'a str,
    pub aud: &'a str,
    pub iat: u64,
    pub exp: u64,
    /// Allowed models (names or prefixes ending in `*`); `None` omits the claim.
    pub models: Option<&'a [&'a str]>,
    /// Largest output-token maximum; `None` omits the claim.
    pub max_output_tokens: Option<u64>,
}

/// The claims as they are signed.
#[derive(Serialize)]
struct Signed<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<&'a [&'a str]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
}

/// Signs `claims` with `key`, under its [`thumbprint`] as the key id.
pub fn issue(key: &Ed25519KeyPair, claims: &NewToken<'_>) -> String {
    let kid = key
        .public_key()
        .as_ref()
        .try_into()
        .map(thumbprint)
        .unwrap_or_default();
    let header = Header {
        alg: Algorithm::EdDsa.as_str(),
        kid: &kid,
        typ: Some("JWT"),
    };
    let claims = Signed {
        iss: OWN_ISSUER,
        sub: claims.sub,
        aud: claims.aud,
        iat: claims.iat,
        exp: claims.exp,
        models: claims.models,
        max_output_tokens: claims.max_output_tokens,
    };
    // Serializing these plain structs cannot fail.
    let header = B64.encode(serde_json::to_vec(&header).unwrap_or_default());
    let claims = B64.encode(serde_json::to_vec(&claims).unwrap_or_default());
    let signed = format!("{header}.{claims}");
    let signature = B64.encode(key.sign(signed.as_bytes()));
    format!("{signed}.{signature}")
}

/// Encodes an Ed25519 public key as `[auth] signing_keys` lists it.
pub fn encode_public_key(key: &[u8]) -> String {
    B64.encode(key)
}

/// An Ed25519 public key's JWK thumbprint (RFC 7638): unpadded base64url of
/// the SHA-256 of its canonical JWK, the required members in order. Wye's
/// tokens name their key by it, so config lists keys without ids.
pub fn thumbprint(key: &[u8; 32]) -> String {
    let jwk = format!(
        r#"{{"crv":"Ed25519","kty":"OKP","x":"{}"}}"#,
        B64.encode(key)
    );
    B64.encode(digest(&SHA256, jwk.as_bytes()))
}
