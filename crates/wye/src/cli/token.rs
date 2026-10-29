//! `wye token`: offline key generation and credential issuance.
//!
//! The signing key never enters the server; only its public half goes into
//! `[auth]`. Key files hold the PKCS#8 document in unpadded base64url.

use std::{
    error::Error,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use gateway_core::{
    credential::{self, NewCredential},
    policy::ModelPattern,
};
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};

/// Writes a new signing key to `path` (never overwriting) and prints the
/// public key for the config.
pub fn keygen(path: &Path) -> Result<(), Box<dyn Error>> {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| "cannot generate a key")?;
    let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| "generated key invalid")?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    writeln!(file, "{}", B64.encode(pkcs8.as_ref()))?;
    println!(
        "signing key written to {} (keep it secret, outside the server)\n\n\
         add to wye.toml:\n[auth]\nissuer = \"wye\"\naudience = \"<your audience>\"\n\
         [[auth.keys]]\nid = \"<choose an id>\"\ned25519_public_key = \"{}\"",
        path.display(),
        credential::encode_public_key(key.public_key().as_ref()),
    );
    Ok(())
}

const ISSUE_FLAGS: [&str; 8] = [
    "--key",
    "--kid",
    "--issuer",
    "--audience",
    "--subject",
    "--ttl-secs",
    "--models",
    "--max-output-tokens",
];

/// Prints a credential signed with the key in `--key`.
pub fn issue(flags: &[&str]) -> Result<(), Box<dyn Error>> {
    let optional = |name: &str| {
        flags.chunks(2).find_map(|pair| match pair {
            [n, value] if *n == name => Some(*value),
            _ => None,
        })
    };
    let flag = |name: &str| -> Result<&str, Box<dyn Error>> {
        optional(name).ok_or_else(|| format!("missing {name}").into())
    };
    if !flags.len().is_multiple_of(2) {
        return Err("flags must be --name value pairs".into());
    }
    // A mistyped optional flag (`--model`) would otherwise issue a credential
    // without the restriction it was meant to carry.
    if let Some(unknown) = flags
        .iter()
        .step_by(2)
        .find(|name| !ISSUE_FLAGS.contains(name))
    {
        return Err(format!("unknown flag {unknown}").into());
    }
    // A repeated flag would leave one of its values unused: `--models`
    // given twice would issue the first, perhaps broader, list.
    let names: Vec<&str> = flags.iter().step_by(2).copied().collect();
    if let Some(repeated) = names
        .iter()
        .enumerate()
        .find_map(|(i, name)| names.get(..i)?.contains(name).then_some(name))
    {
        return Err(format!("{repeated} given more than once").into());
    }
    let key_path = flag("--key")?;
    let encoded =
        std::fs::read_to_string(key_path).map_err(|e| format!("cannot read {key_path}: {e}"))?;
    let pkcs8 = B64
        .decode(encoded.trim())
        .map_err(|_| format!("{key_path}: not a wye signing key"))?;
    let key = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| format!("{key_path}: not an Ed25519 key"))?;
    let ttl: u64 = flag("--ttl-secs")?
        .parse()
        .map_err(|_| "--ttl-secs must be a number of seconds")?;
    let models: Option<Vec<&str>> = optional("--models").map(|list| list.split(',').collect());
    if let Some(list) = &models
        && let Some(bad) = list.iter().find(|p| ModelPattern::parse(p).is_none())
    {
        return Err(
            format!("--models: {bad:?} is not a model name or a prefix ending in *").into(),
        );
    }
    let max_output_tokens = optional("--max-output-tokens")
        .map(|n| match n.parse::<u64>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err("--max-output-tokens must be a positive number"),
        })
        .transpose()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let token = credential::issue(
        &key,
        flag("--kid")?,
        &NewCredential {
            iss: flag("--issuer")?,
            sub: flag("--subject")?,
            aud: flag("--audience")?,
            iat: now,
            exp: now.checked_add(ttl).ok_or("--ttl-secs too large")?,
            models: models.as_deref(),
            max_output_tokens,
        },
    );
    println!("{token}");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_and_repeated_flags_are_refused_before_anything_is_signed() {
        let error = super::issue(&["--key", "k", "--model", "claude-haiku-*"]).unwrap_err();
        assert_eq!(error.to_string(), "unknown flag --model");
        let flags = [
            "--models",
            "claude-*",
            "--key",
            "k",
            "--models",
            "claude-haiku-*",
        ];
        let error = super::issue(&flags).unwrap_err();
        assert_eq!(error.to_string(), "--models given more than once");
    }
}
