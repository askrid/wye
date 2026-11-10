//! Gateway tokens and the `[auth]` section that trusts their key.

use super::*;

/// Unix time in seconds, for gateway-token claims.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Claims the tests' gateway tokens share: audience `wye-prod`, valid for
/// ten minutes, no model list or ceiling.
pub(crate) fn claims(sub: &str) -> NewToken<'_> {
    NewToken {
        sub,
        aud: "wye-prod",
        iat: now(),
        exp: now() + 600,
        models: None,
        max_output_tokens: None,
    }
}

/// A gateway token for `sub` with its own model list and output ceiling, if any.
pub(crate) fn token(
    key: &Ed25519KeyPair,
    sub: &str,
    models: Option<&[&str]>,
    max_output_tokens: Option<u64>,
) -> String {
    token::issue(
        key,
        &NewToken {
            models,
            max_output_tokens,
            ..claims(sub)
        },
    )
}

/// An `[auth]` section trusting `key`, with `extra` lines.
pub(crate) fn auth_section(key: &Ed25519KeyPair, extra: &str) -> String {
    format!(
        "\n[auth]\naudience = \"wye-prod\"\nsigning_keys = [\"{}\"]\n{extra}",
        token::encode_public_key(key.public_key().as_ref())
    )
}
