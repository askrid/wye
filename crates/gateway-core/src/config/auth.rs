//! `[auth]`: Wye's own signing keys and the identity providers it trusts.

use super::*;

/// Signed gateway tokens (see [`crate::token`]): Wye's own, from
/// identity providers, or both.
#[derive(Debug, Clone)]
pub struct Auth {
    /// Tokens issued with `wye token issue`; `None` when only identity
    /// providers are trusted.
    pub own: Option<OwnIssuer>,
    /// Identity providers whose tokens are accepted directly.
    pub identity_providers: Vec<IdentityProvider>,
    /// Longest accepted `exp - iat`: without a revocation list, offboarding
    /// takes effect when the person's last token expires.
    pub max_gateway_token_ttl: Duration,
    /// Models any token may use; `None` allows every model. A token's
    /// `models` claim, or an issuer's grants, narrow this, never widen it.
    pub models: Option<Vec<ModelPattern>>,
    /// Largest output-token maximum any request may ask for; a token's
    /// `max_output_tokens` claim can only lower it.
    pub max_output_tokens: Option<u64>,
}

/// The `iss` claim of Wye's own tokens.
pub const OWN_ISSUER: &str = "wye";

/// Wye's own tokens: Ed25519, `alg` pinned to `EdDSA`, `iss` always
/// [`OWN_ISSUER`].
#[derive(Debug, Clone)]
pub struct OwnIssuer {
    /// Required `aud` claim (one of them, if a list).
    pub audience: String,
    /// Verification keys, selected by the token's `kid`, each key's
    /// thumbprint; several allow rotation.
    pub keys: Vec<PublicKey>,
}

/// One of Wye's own signing keys, by its public half.
#[derive(Debug, Clone)]
pub struct PublicKey {
    pub ed25519_public_key: [u8; 32],
    /// The key's RFC 7638 JWK thumbprint: the `kid` of the tokens it signs.
    pub thumbprint: String,
}

impl PublicKey {
    /// A key as config writes it: 32 bytes in unpadded base64url.
    pub(crate) fn decode(encoded: &str) -> Option<Self> {
        let ed25519_public_key = crate::token::decode_public_key(encoded)?;
        Some(Self::new(ed25519_public_key))
    }

    pub fn new(ed25519_public_key: [u8; 32]) -> Self {
        Self {
            thumbprint: crate::token::thumbprint(&ed25519_public_key),
            ed25519_public_key,
        }
    }
}

/// An identity provider (`[[auth.identity_providers]]`) whose JWTs Wye verifies
/// directly, with keys pinned in config: no runtime key fetching.
#[derive(Debug, Clone)]
pub struct IdentityProvider {
    /// Required `iss` claim.
    pub issuer: String,
    /// Required `aud` claim (one of them, if a list).
    pub audience: String,
    /// The one algorithm accepted from this issuer, never read from the token.
    pub algorithm: Algorithm,
    /// The issuer's JSON Web Key Set, relative to the config file; read at startup.
    pub jwks_file: PathBuf,
    /// The claim that names the token's subject (default `sub`).
    pub subject_claim: String,
    /// Models granted by claim values. Empty: the `[auth] models` ceiling
    /// applies alone. Otherwise a token gets the union of its matching
    /// grants, and one matching none may use no model.
    pub grants: Vec<Grant>,
}

/// Signature algorithms accepted from identity providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// RSASSA-PKCS1-v1_5 with SHA-256, keys of 2048 to 8192 bits.
    Rs256,
    /// ECDSA on P-256 with SHA-256.
    Es256,
    /// Ed25519.
    EdDsa,
}

impl Algorithm {
    pub(crate) const ALL: [Self; 3] = [Self::Rs256, Self::Es256, Self::EdDsa];

    /// The JOSE `alg` name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Es256 => "ES256",
            Self::EdDsa => "EdDSA",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.as_str() == name)
    }
}

/// Models a token gets when its `claim` equals `value`, or is a list
/// containing it (for example `groups`).
#[derive(Debug, Clone)]
pub struct Grant {
    pub claim: String,
    pub value: String,
    pub models: Vec<ModelPattern>,
}
