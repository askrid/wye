//! `wye token`: offline key generation and gateway token issuance.
//!
//! The signing key never enters the server; only its public half goes into
//! `[auth]`. Key files hold the PKCS#8 document in unpadded base64url.

use std::path::Path;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use gateway_core::{
    config::{Auth, Config, OwnIssuer},
    duration::show,
    duration_syntax,
    policy::ModelPattern,
    token::{self, NewToken},
};
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};

use super::{
    Env, Error, Exit,
    args::Parsed,
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
    let line = format!("{}\n", B64.encode(pkcs8.as_ref()));
    super::write_new(path, &line, true, "--out", &|p| p.display().to_string())?;
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

/// The key's public half as `[auth] signing_keys` lists it.
pub fn public_key(key: &Ed25519KeyPair) -> String {
    token::encode_public_key(key.public_key().as_ref())
}

/// `wye token keygen`: a new key, written; the result is the `signing_keys`
/// line to put under `[auth]`.
pub fn run_keygen(parsed: &Parsed<'_>, env: &impl Env) -> Result<Exit, Error> {
    let out = match parsed.value("out") {
        Some(out) => out.into(),
        None => {
            let out = files::in_home(SIGNING_KEY_FILE, "pass --out <path>", env)?;
            if out.symlink_metadata().is_ok() {
                return Err(format!(
                    "{} exists, and `wye token issue` signs with it; for another key, \
                     as when rotating, pass --out <path>",
                    files::tilde(&out, env)
                )
                .into());
            }
            out
        }
    };
    let key = new_key(&out)?;
    super::tell(&format!(
        "Wrote {} (keep it secret, outside the gateway); add its public half to \
         wye.toml, under [auth] (when rotating, after the keys already in \
         signing_keys, until their tokens expire):\n",
        files::tilde(&out, env),
    ));
    emit(&format!("signing_keys = [\"{}\"]\n", public_key(&key)));
    Ok(Exit::Done)
}

/// `wye token issue`: a token, printed or written to `--out`.
pub fn run_issue(parsed: &Parsed<'_>, env: &impl Env) -> Result<Exit, Error> {
    let ttl_secs = parsed
        .value("ttl")
        .map(|ttl| {
            super::duration_secs(ttl).ok_or_else(|| {
                Error::Usage(format!(
                    concat!("--ttl {}: write it as ", duration_syntax!()),
                    ttl
                ))
            })
        })
        .transpose()?;
    let models = parsed
        .value("models")
        .map(models)
        .transpose()
        .map_err(Error::Usage)?;
    let max_output_tokens = parsed
        .value("max-output-tokens")
        .map(ceiling)
        .transpose()
        .map_err(Error::Usage)?;
    // The config first: one that takes no tokens of Wye's needs no key.
    let (path, config) = files::load_config(parsed, env)?;
    files::named(&path, issuer(&config))?;
    let key = match parsed.value("signing-key") {
        Some(key) => key.into(),
        None => {
            let key = files::in_home(SIGNING_KEY_FILE, "pass --signing-key <path>", env)?;
            if key.symlink_metadata().is_err() {
                return Err(format!(
                    "no signing key at {}: pass --signing-key <path>, or make one with \
                     `wye token keygen` and add its public half to [auth] signing_keys",
                    files::tilde(&key, env)
                )
                .into());
            }
            key
        }
    };
    let request = Issue {
        subject: parsed.positional.first().copied().unwrap_or_default(),
        ttl_secs,
        models,
        max_output_tokens,
    };
    let key = read_key(&key)?;
    let issued = files::named(&path, issue(&request, &key, &config, super::unix_secs()?))?;
    match parsed.value("out") {
        Some(out) => {
            let line = format!("{issued}\n");
            super::write_new(Path::new(out), &line, true, "--out", &|p| {
                p.display().to_string()
            })?;
            super::tell(&format!("Wrote {out}, readable only by you\n"));
        }
        None => emit(&format!("{issued}\n")),
    }
    Ok(Exit::Done)
}

/// `--models`: comma-separated model names or prefixes ending in `*`.
fn models(list: &str) -> Result<Vec<&str>, String> {
    let models: Vec<&str> = list.split(',').collect();
    match models.iter().find(|p| ModelPattern::parse(p).is_none()) {
        Some(bad) => Err(format!(
            "--models: {bad:?} is not a model name or a prefix ending in *"
        )),
        None => Ok(models),
    }
}

/// `--max-output-tokens`: a positive number.
fn ceiling(n: &str) -> Result<u64, String> {
    match n.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err("--max-output-tokens must be a positive number".into()),
    }
}

/// What `wye token issue` was asked for.
#[derive(Debug, Default)]
pub struct Issue<'a> {
    /// Who the token is for: its `sub`.
    pub subject: &'a str,
    /// Its lifetime, in seconds; `None` for the config's longest.
    pub ttl_secs: Option<u64>,
    /// Model names or prefixes ending in `*`.
    pub models: Option<Vec<&'a str>>,
    /// An output-token ceiling lower than the config's.
    pub max_output_tokens: Option<u64>,
}

/// A token for `issue.subject`, signed with `key`. The audience and longest
/// lifetime are the config's, and the key must be among its signing keys,
/// so the gateway that reads this config accepts it. Errors from here are
/// about the config.
pub fn issue(
    issue: &Issue<'_>,
    key: &Ed25519KeyPair,
    config: &Config,
    now: u64,
) -> Result<String, String> {
    let (auth, own) = issuer(config)?;
    let public = key.public_key().as_ref();
    if !own
        .keys
        .iter()
        .any(|k| k.ed25519_public_key.as_slice() == public)
    {
        return Err(format!(
            "the signing key is not among its [auth] signing_keys; add its public half: {}",
            public_key(key)
        ));
    }
    let longest = auth.max_gateway_token_ttl;
    let ttl = issue.ttl_secs.unwrap_or(longest.as_secs());
    if ttl > longest.as_secs() {
        return Err(format!(
            "--ttl {} is longer than it accepts ({}, max_gateway_token_ttl)",
            show(std::time::Duration::from_secs(ttl)),
            show(longest)
        ));
    }
    Ok(token::issue(
        key,
        &NewToken {
            sub: issue.subject,
            aud: &own.audience,
            iat: now,
            exp: now.checked_add(ttl).ok_or("--ttl too large")?,
            models: issue.models.as_deref(),
            max_output_tokens: issue.max_output_tokens,
        },
    ))
}

/// The config's `[auth]` and its own issuer, which signs Wye tokens; errors
/// say why the config takes none.
fn issuer(config: &Config) -> Result<(&Auth, &OwnIssuer), &'static str> {
    let auth = config.auth.as_ref().ok_or(
        "it has no [auth], so its gateway takes no gateway tokens; name another config \
         with -c, or write one with `wye init --shared`",
    )?;
    let own = auth.own.as_ref().ok_or(
        "its [auth] trusts only identity providers, so it takes no Wye tokens; name \
         another config with -c",
    )?;
    Ok((auth, own))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn config(extra: &str, public: &str) -> Config {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"0.0.0.0:8080\"\n\
             [[upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key = {{ env = \"K\" }}\n\
             [auth]\naudience = \"team\"\n{extra}\nsigning_keys = [\"{}\", \"{public}\"]\n",
            "A".repeat(43),
        ))
        .unwrap()
    }

    #[test]
    fn issue_takes_claims_from_the_config_and_names_the_key_by_its_thumbprint() {
        let dir = wye_testkit::TempDir::new("token");
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
        let config = config("max_gateway_token_ttl = \"1h\"", &public_key(&key));
        let alice = Issue {
            subject: "alice",
            ..Issue::default()
        };
        let token = issue(&alice, &key, &config, 1000).unwrap();
        let header = B64.decode(token.split('.').next().unwrap()).unwrap();
        let kid = token::thumbprint(key.public_key().as_ref().try_into().unwrap());
        assert!(
            String::from_utf8_lossy(&header).contains(&format!("\"kid\":\"{kid}\"")),
            "{kid}"
        );
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
            "--ttl 2h is longer than it accepts (1h, max_gateway_token_ttl)"
        );
        assert!(models("claude-*,a*b").unwrap_err().contains("a*b"));
        assert_eq!(models("claude-*,m").unwrap(), ["claude-*", "m"]);
        assert!(ceiling("0").is_err() && ceiling("8000") == Ok(8000));
        let (other, _) = signing_key(&dir.join("other.key")).unwrap();
        let unknown = issue(&Issue::default(), &other, &config, 0).unwrap_err();
        assert!(unknown.contains(&public_key(&other)), "{unknown}");
        let no_auth = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\
             [[upstreams]]\nbase_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n",
        )
        .unwrap();
        assert!(
            issue(&Issue::default(), &key, &no_auth, 0)
                .unwrap_err()
                .contains("no [auth]")
        );
    }
}
