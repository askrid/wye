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
use gateway_core::credential::{self, NewCredential};
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
         add to wye.toml:\n[[auth.keys]]\nid = \"<choose an id>\"\n\
         ed25519_public_key = \"{}\"",
        path.display(),
        credential::encode_public_key(key.public_key().as_ref()),
    );
    Ok(())
}

/// Prints a credential signed with the key in `--key`.
pub fn issue(flags: &[&str]) -> Result<(), Box<dyn Error>> {
    let flag = |name: &str| -> Result<&str, Box<dyn Error>> {
        flags
            .chunks(2)
            .find_map(|pair| match pair {
                [n, value] if *n == name => Some(*value),
                _ => None,
            })
            .ok_or_else(|| format!("missing {name}").into())
    };
    if !flags.len().is_multiple_of(2) {
        return Err("flags must be --name value pairs".into());
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
        },
    );
    println!("{token}");
    Ok(())
}
