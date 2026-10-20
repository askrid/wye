//! Identity-provider tokens for tests: signing keys for RS256, ES256, and
//! EdDSA, their JWKS entries, and compact JWTs with any header and claims.
//!
//! ring cannot generate RSA keys, so RS256 uses two fixed 2048-bit keys
//! (`rsa-test-key*.pk8`, PKCS#8 DER, made with `openssl genpkey`). They sign
//! test tokens only and protect nothing.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::{
    rand::SystemRandom,
    signature::{
        ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair, RSA_PKCS1_SHA256,
        RsaKeyPair, RsaPublicKeyComponents,
    },
};

const RSA_TEST_KEY: &[u8] = include_bytes!("rsa-test-key.pk8");
const OTHER_RSA_TEST_KEY: &[u8] = include_bytes!("rsa-test-key-2.pk8");

/// A signing key of an identity provider under test.
pub enum IdpKey {
    Rs256(RsaKeyPair),
    Es256(EcdsaKeyPair),
    EdDsa(Ed25519KeyPair),
}

impl IdpKey {
    /// The fixed RSA test key.
    pub fn rs256() -> Self {
        Self::Rs256(RsaKeyPair::from_pkcs8(RSA_TEST_KEY).expect("RSA test key"))
    }

    /// A second fixed RSA key, for forgeries.
    pub fn rs256_other() -> Self {
        Self::Rs256(RsaKeyPair::from_pkcs8(OTHER_RSA_TEST_KEY).expect("RSA test key"))
    }

    /// A fresh P-256 key.
    pub fn es256() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .expect("P-256 key");
        Self::Es256(
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                .expect("P-256 key"),
        )
    }

    /// A fresh Ed25519 key.
    pub fn ed25519() -> Self {
        Self::EdDsa(crate::ed25519_keypair())
    }

    /// The JOSE `alg` name.
    pub fn alg(&self) -> &'static str {
        match self {
            Self::Rs256(_) => "RS256",
            Self::Es256(_) => "ES256",
            Self::EdDsa(_) => "EdDSA",
        }
    }

    /// This key's public JWK, as an identity provider publishes it.
    pub fn jwk(&self, kid: &str) -> String {
        match self {
            Self::Rs256(key) => {
                let public = RsaPublicKeyComponents::<Vec<u8>>::from(key.public());
                let (n, e) = (public.n, public.e);
                format!(
                    r#"{{"kty":"RSA","use":"sig","alg":"RS256","kid":"{kid}","n":"{}","e":"{}"}}"#,
                    B64.encode(n),
                    B64.encode(e)
                )
            }
            Self::Es256(key) => {
                // Uncompressed point: 0x04 || x || y.
                let point = key.public_key().as_ref();
                let (x, y) = point.get(1..).expect("P-256 point").split_at(32);
                format!(
                    r#"{{"kty":"EC","crv":"P-256","kid":"{kid}","x":"{}","y":"{}"}}"#,
                    B64.encode(x),
                    B64.encode(y)
                )
            }
            Self::EdDsa(key) => format!(
                r#"{{"kty":"OKP","crv":"Ed25519","kid":"{kid}","x":"{}"}}"#,
                B64.encode(key.public_key().as_ref())
            ),
        }
    }

    /// A compact JWT over `header` and `claims`, both JSON objects as written.
    pub fn sign(&self, header: &str, claims: &str) -> String {
        let signed = format!("{}.{}", B64.encode(header), B64.encode(claims));
        let rng = SystemRandom::new();
        let signature = match self {
            Self::Rs256(key) => {
                let mut signature = vec![0; key.public().modulus_len()];
                key.sign(&RSA_PKCS1_SHA256, &rng, signed.as_bytes(), &mut signature)
                    .expect("RSA signature");
                signature
            }
            Self::Es256(key) => key
                .sign(&rng, signed.as_bytes())
                .expect("ECDSA signature")
                .as_ref()
                .to_vec(),
            Self::EdDsa(key) => key.sign(signed.as_bytes()).as_ref().to_vec(),
        };
        format!("{signed}.{}", B64.encode(signature))
    }

    /// A token with the usual header (`alg`, `kid`, `typ`).
    pub fn token(&self, kid: &str, claims: &str) -> String {
        self.sign(
            &format!(r#"{{"alg":"{}","kid":"{kid}","typ":"JWT"}}"#, self.alg()),
            claims,
        )
    }
}

/// A JWKS document holding `keys` (key, kid).
pub fn jwks(keys: &[(&IdpKey, &str)]) -> String {
    let keys: Vec<String> = keys.iter().map(|(key, kid)| key.jwk(kid)).collect();
    format!(r#"{{"keys":[{}]}}"#, keys.join(","))
}
