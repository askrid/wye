//! Issuing Wye's own tokens, offline (`wye token issue`): the gateway only
//! verifies them.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::signature::Ed25519KeyPair;
use serde::Serialize;

use super::Header;
use crate::config::Algorithm;

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
