//! `wye token`: offline key generation and gateway token issuance.
//!
//! The signing key never enters the server; only its public half goes into
//! `[auth]`. Key files hold the PKCS#8 document in unpadded base64url.

use std::path::Path;

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

use super::{
    args::{self, Parsed, show_duration},
    emit,
    files::{self, SIGNING_KEY_FILE},
};

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
    super::write_secret(path, &B64.encode(pkcs8.as_ref()))?;
    Ok(key)
}

/// The signing key in the file `path`.
pub fn read_key(path: &Path) -> Result<Ed25519KeyPair, String> {
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
pub fn run_keygen(
    parsed: &Parsed<'_>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let out = match parsed.value("out") {
        Some(out) => out.into(),
        None => files::in_home(SIGNING_KEY_FILE, "pass --out", env)?,
    };
    let key = new_key(&out)?;
    Ok(format!(
        "wrote {} (keep it secret, outside the gateway)\n\n\
         Add to wye.toml, under [auth]:\n[[auth.keys]]\nid = \"<a new id>\"\n\
         ed25519_public_key = \"{}\"\n",
        out.display(),
        public_key(&key),
    ))
}

/// `wye token issue`, from its options.
pub fn run_issue(parsed: &Parsed<'_>, env: &impl Fn(&str) -> Option<String>) -> Result<(), String> {
    let key = match parsed.value("key") {
        Some(key) => key.into(),
        None => files::in_home(SIGNING_KEY_FILE, "pass --key", env)?,
    };
    let ttl_secs = parsed
        .value("ttl")
        .map(|ttl| {
            args::duration_secs(ttl)
                .ok_or_else(|| format!("--ttl {ttl}: write it as 30m, 8h, or 7d"))
        })
        .transpose()?;
    let request = Issue {
        subject: parsed.positional.first().copied().unwrap_or_default(),
        ttl_secs,
        models: parsed.value("models"),
        max_output_tokens: parsed.value("max-output-tokens"),
    };
    let key = read_key(&key)?;
    let path = files::config_path(parsed, env)?;
    let config = files::load(&path)?;
    let issued = files::named(&path, issue(&request, &key, &config, super::unix_secs()?))?;
    match parsed.value("out") {
        Some(out) => {
            super::write_secret(Path::new(out), &issued)?;
            eprintln!("wrote {out}, readable only by you");
        }
        None => emit(&format!("{issued}\n")),
    }
    Ok(())
}

/// What `wye token issue` was asked for.
#[derive(Debug, Default)]
pub struct Issue<'a> {
    pub subject: &'a str,
    pub ttl_secs: Option<u64>,
    /// Comma-separated model names or prefixes ending in `*`.
    pub models: Option<&'a str>,
    pub max_output_tokens: Option<&'a str>,
}

/// A token for `issue.subject`, signed with `key`. The issuer, audience, key
/// id, and longest lifetime are the config's, so the gateway that reads this
/// config accepts it. Errors from here are about the config.
pub fn issue(
    issue: &Issue<'_>,
    key: &Ed25519KeyPair,
    config: &Config,
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
    let auth = config
        .auth
        .as_ref()
        .ok_or("it has no [auth], so its gateway takes no tokens; name another with -c")?;
    let own = auth
        .own
        .as_ref()
        .ok_or("its [auth] has no issuer and keys of its own; name another with -c")?;
    let public = key.public_key().as_ref();
    let kid = own
        .keys
        .iter()
        .find(|k| k.ed25519_public_key.as_slice() == public)
        .map(|k| k.id.as_str())
        .ok_or_else(|| {
            format!(
                "the signing key is not among its [[auth.keys]]; add its public half: {}",
                public_key(key)
            )
        })?;
    let longest = auth.max_token_ttl.as_secs();
    let ttl = issue.ttl_secs.unwrap_or(longest);
    if ttl > longest {
        return Err(format!(
            "--ttl {} is longer than it accepts ({}, max_token_ttl_secs)",
            show_duration(ttl),
            show_duration(longest)
        ));
    }
    Ok(credential::issue(
        key,
        kid,
        &NewCredential {
            iss: &own.issuer,
            sub: issue.subject,
            aud: &own.audience,
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

    fn config(extra: &str, public: &str) -> Config {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"0.0.0.0:8080\"\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\n\
             [auth]\nissuer = \"wye\"\naudience = \"team\"\n{extra}\n\
             [[auth.keys]]\nid = \"old\"\ned25519_public_key = \"{}\"\n\
             [[auth.keys]]\nid = \"now\"\ned25519_public_key = \"{public}\"\n",
            "A".repeat(43),
        ))
        .unwrap()
    }

    #[test]
    fn issue_takes_claims_and_the_key_id_from_the_config() {
        let dir = std::env::temp_dir().join(format!("wye-token-{}", std::process::id()));
        // keygen's default path sits in a home that may not exist yet.
        let path = dir.join("new-home/issuer.key");
        let (key, created) = signing_key(&path).unwrap();
        assert!(created);
        assert!(!signing_key(&path).unwrap().1, "an existing key is reused");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        let config = config("max_token_ttl_secs = 3600", &public_key(&key));
        let alice = Issue {
            subject: "alice",
            ..Issue::default()
        };
        let token = issue(&alice, &key, &config, 1000).unwrap();
        let header = B64.decode(token.split('.').next().unwrap()).unwrap();
        assert!(String::from_utf8_lossy(&header).contains("\"kid\":\"now\""));
        let claims = B64.decode(token.split('.').nth(1).unwrap()).unwrap();
        let claims = String::from_utf8_lossy(&claims);
        assert!(claims.contains("\"aud\":\"team\""), "{claims}");
        assert!(
            claims.contains("\"exp\":4600"),
            "the config's longest: {claims}"
        );

        let long = Issue {
            ttl_secs: Some(7200),
            ..alice
        };
        assert_eq!(
            issue(&long, &key, &config, 0).unwrap_err(),
            "--ttl 2h is longer than it accepts (1h, max_token_ttl_secs)"
        );
        let bad = Issue {
            models: Some("claude-*,a*b"),
            ..Issue::default()
        };
        assert!(issue(&bad, &key, &config, 0).unwrap_err().contains("a*b"));
        let (other, _) = signing_key(&dir.join("other.key")).unwrap();
        let unknown = issue(&Issue::default(), &other, &config, 0).unwrap_err();
        assert!(unknown.contains(&public_key(&other)), "{unknown}");
        let no_auth = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n",
        )
        .unwrap();
        assert!(
            issue(&Issue::default(), &key, &no_auth, 0)
                .unwrap_err()
                .contains("no [auth]")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
