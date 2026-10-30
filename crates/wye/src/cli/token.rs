//! `wye token`: offline key generation and gateway token issuance.
//!
//! The signing key never enters the server; only its public half goes into
//! `[auth]`. Key files hold the PKCS#8 document in unpadded base64url.

use std::{io::Write, path::Path};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use gateway_core::{
    config::Config,
    credential::{self, NewCredential},
    policy::ModelPattern,
};
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};

use super::args::show_duration;

/// The signing key in `path`, made first if the file does not exist; `true`
/// when it was made.
pub fn signing_key(path: &Path) -> Result<(Ed25519KeyPair, bool), String> {
    match read_key(path) {
        Ok(key) => Ok((key, false)),
        Err(_) if path.symlink_metadata().is_err() => Ok((new_key(path)?, true)),
        Err(e) => Err(e),
    }
}

/// Writes a new signing key to `path`, never over an existing file,
/// readable only by its owner.
pub fn new_key(path: &Path) -> Result<Ed25519KeyPair, String> {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| "cannot generate a key")?;
    let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| "generated key invalid")?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
            format!("{} exists; choose another path with --out", path.display())
        }
        _ => format!("cannot create {}: {e}", path.display()),
    })?;
    writeln!(file, "{}", B64.encode(pkcs8.as_ref()))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(key)
}

fn read_key(path: &Path) -> Result<Ed25519KeyPair, String> {
    let shown = path.display();
    let encoded = std::fs::read_to_string(path).map_err(|e| format!("cannot read {shown}: {e}"))?;
    let pkcs8 = B64
        .decode(encoded.trim())
        .map_err(|_| format!("{shown}: not a wye signing key"))?;
    Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| format!("{shown}: not an Ed25519 key"))
}

/// The key's public half as `[[auth.keys]]` takes it.
pub fn public_key(key: &Ed25519KeyPair) -> String {
    credential::encode_public_key(key.public_key().as_ref())
}

/// `wye token keygen`: a new key, and what to add to the config.
pub fn keygen(path: &Path) -> Result<String, String> {
    let key = new_key(path)?;
    Ok(format!(
        "wrote {} (keep it secret, outside the server)\n\n\
         Add to wye.toml, under [auth]:\n[[auth.keys]]\nid = \"<a new id>\"\n\
         ed25519_public_key = \"{}\"\n",
        path.display(),
        public_key(&key),
    ))
}

/// What `wye token issue` was asked for.
#[derive(Debug, Default)]
pub struct Issue<'a> {
    pub subject: &'a str,
    pub ttl_secs: Option<u64>,
    /// Comma-separated model names or prefixes ending in `*`.
    pub models: Option<&'a str>,
    pub max_output_tokens: Option<&'a str>,
    /// Without a config: the claims and key id it would give.
    pub issuer: Option<&'a str>,
    pub audience: Option<&'a str>,
    pub kid: Option<&'a str>,
}

/// A token for `issue.subject`, signed with the key in `key_path`. The
/// issuer, audience, key id, and longest lifetime come from `config`'s
/// `[auth]` unless given.
pub fn issue(
    issue: &Issue<'_>,
    key_path: &Path,
    config: Option<&Config>,
    now: u64,
) -> Result<String, String> {
    let models: Option<Vec<&str>> = issue.models.map(|list| list.split(',').collect());
    if let Some(list) = &models
        && let Some(bad) = list.iter().find(|p| ModelPattern::parse(p).is_none())
    {
        return Err(format!(
            "--models: {bad:?} is not a model name or a prefix ending in *"
        ));
    }
    let max_output_tokens = issue
        .max_output_tokens
        .map(|n| match n.parse::<u64>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err("--max-output-tokens must be a positive number"),
        })
        .transpose()?;
    let key = read_key(key_path)?;
    let auth = config.and_then(|c| c.auth.as_ref());
    let own = auth.and_then(|a| a.own.as_ref());
    let missing = |what: &str, flag: &str| match config {
        Some(_) => format!("it has no [auth] {what}; name another with -c, or pass --{flag}"),
        None => format!("no config to read the {what} from; pass --{flag} or -c"),
    };
    let issuer = issue
        .issuer
        .or(own.map(|o| o.issuer.as_str()))
        .ok_or_else(|| missing("issuer", "issuer"))?;
    let audience = issue
        .audience
        .or(own.map(|o| o.audience.as_str()))
        .ok_or_else(|| missing("audience", "audience"))?;
    let public = key.public_key().as_ref();
    let kid = match (issue.kid, own) {
        (Some(kid), _) => kid,
        (None, Some(own)) => own
            .keys
            .iter()
            .find(|k| k.ed25519_public_key.as_slice() == public)
            .map(|k| k.id.as_str())
            .ok_or_else(|| {
                format!(
                    "{} is not among the config's [[auth.keys]]; add its public half: {}",
                    key_path.display(),
                    public_key(&key)
                )
            })?,
        (None, None) => return Err(missing("keys", "kid")),
    };
    let longest = auth.map(|a| a.max_credential_ttl.as_secs());
    let ttl = issue
        .ttl_secs
        .or(longest)
        .ok_or("no config to take the lifetime from; pass --ttl")?;
    if let Some(longest) = longest
        && ttl > longest
    {
        return Err(format!(
            "--ttl {} is longer than the config accepts ({}, max_credential_ttl_secs)",
            show_duration(ttl),
            show_duration(longest)
        ));
    }
    Ok(credential::issue(
        &key,
        kid,
        &NewCredential {
            iss: issuer,
            sub: issue.subject,
            aud: audience,
            iat: now,
            exp: now.checked_add(ttl).ok_or("--ttl too large")?,
            models: models.as_deref(),
            max_output_tokens,
        },
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn issue_takes_claims_and_the_key_id_from_the_config() {
        let dir = std::env::temp_dir().join(format!("wye-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("issuer.key");
        let (key, created) = signing_key(&path).unwrap();
        assert!(created);
        assert!(!signing_key(&path).unwrap().1, "an existing key is reused");
        let config = Config::from_toml_str(&format!(
            "[server]\nlisten = \"0.0.0.0:8080\"\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
             [auth]\nissuer = \"wye\"\naudience = \"team\"\nmax_credential_ttl_secs = 3600\n\
             [[auth.keys]]\nid = \"old\"\ned25519_public_key = \"{}\"\n\
             [[auth.keys]]\nid = \"now\"\ned25519_public_key = \"{}\"\n",
            "A".repeat(43),
            public_key(&key)
        ))
        .unwrap();
        let alice = Issue {
            subject: "alice",
            ..Issue::default()
        };
        let token = issue(&alice, &path, Some(&config), 1000).unwrap();
        let header = B64.decode(token.split('.').next().unwrap()).unwrap();
        assert!(String::from_utf8_lossy(&header).contains("\"kid\":\"now\""));
        let claims = B64.decode(token.split('.').nth(1).unwrap()).unwrap();
        let claims = String::from_utf8_lossy(&claims);
        assert!(claims.contains("\"aud\":\"team\""), "{claims}");
        assert!(claims.contains("\"exp\":4600"), "{claims}");

        let long = Issue {
            ttl_secs: Some(7200),
            ..alice
        };
        let error = issue(&long, &path, Some(&config), 0).unwrap_err();
        assert_eq!(
            error,
            "--ttl 2h is longer than the config accepts (1h, max_credential_ttl_secs)"
        );
        let error = issue(&alice, &path, None, 0).unwrap_err();
        assert!(error.contains("pass --issuer or -c"), "{error}");
        let bad = Issue {
            models: Some("claude-*,a*b"),
            ..alice
        };
        assert!(
            issue(&bad, &path, Some(&config), 0)
                .unwrap_err()
                .contains("a*b")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
