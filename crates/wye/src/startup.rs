//! Resolves what the configuration references: secrets from the environment
//! and files from disk. Runs once, before serving.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use gateway_core::{
    config::{Config, IdentityProvider, ProviderKey, Secret, Upstream},
    prompt::HashKey,
    token::{IssuerKeys, KeySetError, Verifier},
};
use rustls::pki_types::{CertificateDer, pem::PemObject};

use http::{HeaderMap, HeaderValue};

/// Failures that prevent the gateway from starting.
#[derive(Debug)]
pub enum StartupError {
    /// A secret's environment variable is not set, or not UTF-8.
    MissingEnv(String),
    /// A secret's environment variable is set but blank.
    EmptyEnv(String),
    /// A header value from config, by the setting it comes from, holds
    /// characters no HTTP header allows.
    InvalidHeader(&'static str),
    /// The cache diagnostics key, from this source, is too short.
    HashKeyTooShort(Secret),
    /// A file the configuration names, by the setting that names it.
    File {
        what: String,
        path: PathBuf,
        reason: String,
    },
    /// The OTLP headers, from this source, are not `key=value,...`.
    InvalidOtlpHeaders(Secret),
    /// An identity provider's key set could not be used.
    KeySet(KeySetError),
    /// The TLS client could not be set up.
    Tls(String),
    /// `server.max_request_body_bytes` does not fit this platform's `usize`.
    BodyLimitTooLarge,
    /// The resources were not resolved from this configuration.
    UpstreamCount,
    /// A problem with a secret of a config section, under its name.
    InSection {
        section: &'static str,
        error: Box<StartupError>,
    },
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv(name) => write!(
                f,
                "environment variable {name} is not set or not valid UTF-8"
            ),
            Self::EmptyEnv(name) => write!(f, "environment variable {name} is empty"),
            Self::InvalidHeader(what) => write!(
                f,
                "{what} contains characters not allowed in an HTTP header \
                 (a trailing newline, for example)"
            ),
            Self::HashKeyTooShort(source) => write!(
                f,
                "hash_key {source} must hold at least {} bytes (for example \
                 `openssl rand -hex 32`)",
                HashKey::MIN_LEN
            ),
            Self::InvalidOtlpHeaders(source) => write!(
                f,
                "otlp_headers {source} must hold key=value pairs separated by commas"
            ),
            Self::File { what, path, reason } => {
                write!(f, "{what} {}: {reason}", path.display())
            }
            Self::KeySet(e) => write!(f, "auth: {e}"),
            Self::Tls(msg) => write!(f, "TLS setup failed: {msg}"),
            Self::BodyLimitTooLarge => {
                f.write_str("server.max_request_body_bytes exceeds this platform's address space")
            }
            Self::UpstreamCount => {
                f.write_str("one set of upstream resources is needed per configured upstream")
            }
            Self::InSection { section, error } => write!(f, "{section}: {error}"),
        }
    }
}

impl std::error::Error for StartupError {}

impl StartupError {
    /// A file error of `upstream` with its setting named by its place in
    /// the config, as validation errors name it.
    fn in_upstream(self, upstream: &Upstream) -> Self {
        match self {
            Self::File { what, path, reason } => Self::File {
                what: format!("upstreams {:?}: {what}", upstream.id),
                path,
                reason,
            },
            other => other,
        }
    }
}

/// Secrets and files the configuration references, loaded at startup.
#[derive(Debug)]
pub struct Resources {
    /// One per upstream, in `Routes::upstreams` order: the Claude route's,
    /// then the OpenAI route's.
    pub upstreams: Vec<UpstreamResources>,
    /// Gateway credential checks, when `[auth]` is configured.
    pub verifier: Option<Verifier>,
    /// Keys the prompt hashes in the usage log; present when cache diagnostics are on.
    pub prompt_key: Option<HashKey>,
    /// Extra headers for OTLP export requests, from `telemetry.otlp_headers`.
    pub otlp_headers: HeaderMap,
}

/// What one upstream's configuration references.
#[derive(Debug)]
pub struct UpstreamResources {
    /// The credential Wye presents upstream.
    pub auth: UpstreamAuth,
    /// Root certificates trusted in addition to the built-in roots.
    pub extra_roots: Vec<CertificateDer<'static>>,
}

impl UpstreamResources {
    /// No extra roots.
    pub fn new(auth: UpstreamAuth) -> Self {
        Self {
            auth,
            extra_roots: Vec::new(),
        }
    }
}

impl Resources {
    /// Loads what `config` references. Relative paths resolve against `config_dir`.
    pub fn resolve(config: &Config, config_dir: &Path) -> Result<Self, StartupError> {
        let mut upstreams = Vec::with_capacity(config.routes.upstreams().count());
        for upstream in config.routes.upstreams() {
            let located = |e: StartupError| e.in_upstream(upstream);
            let auth = upstream_auth(upstream, config_dir).map_err(located)?;
            let extra_roots = upstream_roots(upstream, config_dir).map_err(located)?;
            upstreams.push(UpstreamResources { auth, extra_roots });
        }
        let prompt_key = config
            .cache_diagnostics
            .as_ref()
            .map(|diagnostics| prompt_key(&diagnostics.hash_key, config_dir))
            .transpose()
            .map_err(|error| StartupError::InSection {
                section: "cache_diagnostics",
                error: Box::new(error),
            })?;
        let otlp_headers = match config
            .telemetry
            .as_ref()
            .and_then(|t| t.otlp_headers.as_ref())
        {
            Some(source) => {
                otlp_headers(source, config_dir).map_err(|error| StartupError::InSection {
                    section: "telemetry",
                    error: Box::new(error),
                })?
            }
            None => HeaderMap::new(),
        };
        Ok(Self {
            upstreams,
            verifier: verifier(config, config_dir)?,
            prompt_key,
            otlp_headers,
        })
    }

    /// Every upstream's extra roots: the OTLP exporter trusts them too.
    pub(crate) fn all_extra_roots(&self) -> Vec<CertificateDer<'static>> {
        let mut roots: Vec<CertificateDer<'static>> = Vec::new();
        for cert in self.upstreams.iter().flat_map(|u| &u.extra_roots) {
            if !roots.contains(cert) {
                roots.push(cert.clone());
            }
        }
        roots
    }
}

/// The credential verifier for `[auth]`, with each identity provider's key
/// set read from its `jwks_file` (relative to `config_dir`). `wye explain`
/// uses it too, so it needs no secrets.
pub fn verifier(config: &Config, config_dir: &Path) -> Result<Option<Verifier>, StartupError> {
    let Some(auth) = &config.auth else {
        return Ok(None);
    };
    let sets = auth
        .identity_providers
        .iter()
        .map(|issuer| key_set(issuer, config_dir))
        .collect::<Result<_, _>>()?;
    Verifier::new(auth, sets)
        .map(Some)
        .map_err(StartupError::KeySet)
}

/// An identity provider's key set, read from its `jwks_file` (relative to
/// `config_dir`).
pub(crate) fn key_set(
    issuer: &IdentityProvider,
    config_dir: &Path,
) -> Result<IssuerKeys, StartupError> {
    let path = config_dir.join(&issuer.jwks_file);
    let error = file_error("auth.identity_providers.jwks_file", &path);
    let jwks = std::fs::read(&path).map_err(|e| error(e.to_string()))?;
    IssuerKeys::parse(&jwks, issuer.algorithm).map_err(|e| error(e.to_string()))
}

/// Every secret the configuration names, with the setting that names it:
/// each upstream's provider key, the cache diagnostics key, and the OTLP
/// headers.
pub(crate) fn secrets(config: &Config) -> Vec<(&'static str, &Secret)> {
    let mut secrets: Vec<(&'static str, &Secret)> = config
        .routes
        .upstreams()
        .filter_map(|u| match &u.provider_key {
            ProviderKey::Secret(secret) => Some(("provider_key", secret)),
            ProviderKey::ForwardCaller => None,
        })
        .collect();
    secrets.extend(
        config
            .cache_diagnostics
            .as_ref()
            .map(|d| ("cache_diagnostics.hash_key", &d.hash_key)),
    );
    secrets.extend(
        config
            .telemetry
            .as_ref()
            .and_then(|t| t.otlp_headers.as_ref())
            .map(|headers| ("telemetry.otlp_headers", headers)),
    );
    secrets
}

/// The environment variables [`Resources::resolve`] reads secrets from,
/// sorted: a service does not inherit the shell's environment, so `wye
/// service` refuses a config that names any.
pub(crate) fn env_secrets(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = secrets(config)
        .into_iter()
        .filter_map(|(_, secret)| match secret {
            Secret::Env(name) => Some(name.clone()),
            Secret::File(_) => None,
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

// One resolver per secret, so `wye doctor` checks exactly what `serve`
// will do with it.

/// The credential Wye presents to `upstream`: its key from the environment
/// or a file (relative to `config_dir`), or the caller's own. A file error
/// names the setting relative to the upstream.
pub(crate) fn upstream_auth(
    upstream: &Upstream,
    config_dir: &Path,
) -> Result<UpstreamAuth, StartupError> {
    match &upstream.provider_key {
        ProviderKey::Secret(secret) => {
            let key = read_secret(secret, config_dir, "provider_key")?;
            if upstream.provider.bearer_key() {
                UpstreamAuth::bearer(&key)
            } else {
                UpstreamAuth::api_key(&key)
            }
        }
        ProviderKey::ForwardCaller => Ok(UpstreamAuth::ForwardCaller),
    }
}

/// The key prompt hashes are made with.
pub(crate) fn prompt_key(source: &Secret, config_dir: &Path) -> Result<HashKey, StartupError> {
    let key = read_secret(source, config_dir, "hash_key")?;
    HashKey::new(key.as_bytes()).ok_or_else(|| StartupError::HashKeyTooShort(source.clone()))
}

/// Extra OTLP export headers.
pub(crate) fn otlp_headers(source: &Secret, config_dir: &Path) -> Result<HeaderMap, StartupError> {
    let spec = read_secret(source, config_dir, "otlp_headers")?;
    parse_headers(&spec).ok_or_else(|| StartupError::InvalidOtlpHeaders(source.clone()))
}

/// A secret's value: an environment variable, set and not blank, or a
/// file's contents (relative to `config_dir`) without surrounding
/// whitespace, since mounted secrets often end in a newline. A blank key
/// would start the server and fail every request upstream. `what` names
/// the setting in a file error.
fn read_secret(
    secret: &Secret,
    config_dir: &Path,
    what: &'static str,
) -> Result<String, StartupError> {
    match secret {
        Secret::Env(name) => {
            let value = env(name)?;
            if value.trim().is_empty() {
                return Err(StartupError::EmptyEnv(name.clone()));
            }
            Ok(value)
        }
        Secret::File(path) => {
            let path = config_dir.join(path);
            let error = file_error(what, &path);
            let contents = std::fs::read_to_string(&path).map_err(|e| error(e.to_string()))?;
            let secret = contents.trim();
            if secret.is_empty() {
                return Err(error("file is empty".into()));
            }
            Ok(secret.to_owned())
        }
    }
}

/// Why the secret file `path` is open to more than its owner, with the
/// command that closes it; `None` when only its owner may read or write it
/// (or it cannot be read, which reading it reports).
pub(crate) fn open_to_others(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
        (mode & 0o077 != 0).then(|| {
            let path = path.display();
            format!("{path} is open to group or others (mode {mode:o}): chmod 600 {path}")
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Each secret file (relative to `config_dir`) that group or others may
/// open, and the command that closes it: `serve` warns of each, and `wye
/// doctor` fails on it.
pub fn key_file_warnings(config: &Config, config_dir: &Path) -> Vec<String> {
    secrets(config)
        .into_iter()
        .filter_map(|(what, secret)| match secret {
            Secret::File(path) => open_to_others(&config_dir.join(path)).map(|why| (what, why)),
            Secret::Env(_) => None,
        })
        .map(|(what, why)| format!("{what} {why}"))
        .collect()
}

fn env(name: &str) -> Result<String, StartupError> {
    std::env::var(name).map_err(|_| StartupError::MissingEnv(name.to_owned()))
}

/// The root certificates `upstream` trusts besides the built-in ones.
pub(crate) fn upstream_roots(
    upstream: &Upstream,
    config_dir: &Path,
) -> Result<Vec<CertificateDer<'static>>, StartupError> {
    match &upstream.ca_file {
        Some(path) => load_certificates(&config_dir.join(path)),
        None => Ok(Vec::new()),
    }
}

fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, StartupError> {
    let error = file_error("ca_file", path);
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| error(e.to_string()))?;
    if certs.is_empty() {
        return Err(error("no PEM certificates found".into()));
    }
    Ok(certs)
}

fn file_error(what: &'static str, path: &Path) -> impl Fn(String) -> StartupError {
    move |reason| StartupError::File {
        what: what.to_owned(),
        path: path.to_owned(),
        reason,
    }
}

/// Credential Wye presents upstream.
pub enum UpstreamAuth {
    /// Replace the caller's credential headers with this `x-api-key` value
    /// (Anthropic, Bedrock).
    ApiKey(HeaderValue),
    /// Replace the caller's credential headers with this `Authorization`
    /// value, `Bearer` and the key (OpenAI, Claude Platform on AWS).
    Bearer(HeaderValue),
    /// Forward the caller's credential headers unchanged.
    ForwardCaller,
}

impl UpstreamAuth {
    /// `key` sent as `x-api-key`; `Err` if no header may carry it.
    pub fn api_key(key: &str) -> Result<Self, StartupError> {
        Ok(Self::ApiKey(secret(key)?))
    }

    /// `key` sent as `Authorization: Bearer`; `Err` if no header may carry it.
    pub fn bearer(key: &str) -> Result<Self, StartupError> {
        Ok(Self::Bearer(secret(&format!("Bearer {key}"))?))
    }
}

fn secret(value: &str) -> Result<HeaderValue, StartupError> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| StartupError::InvalidHeader("upstream API key"))?;
    value.set_sensitive(true);
    Ok(value)
}

impl fmt::Debug for UpstreamAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Self::ForwardCaller => f.write_str("ForwardCaller"),
        }
    }
}

/// Parses `key1=value1,key2=value2` (the `OTEL_EXPORTER_OTLP_HEADERS` format)
/// into headers whose values are marked sensitive.
fn parse_headers(spec: &str) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    for pair in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=')?;
        let name = http::HeaderName::from_bytes(name.trim().as_bytes()).ok()?;
        let mut value = HeaderValue::from_str(value.trim()).ok()?;
        value.set_sensitive(true);
        headers.append(name, value);
    }
    Some(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_parse_and_are_sensitive() {
        let headers = parse_headers("api-key=secret, x-team = research").unwrap();
        assert_eq!(headers["api-key"], "secret");
        assert!(headers["api-key"].is_sensitive());
        assert_eq!(headers["x-team"], "research");
        assert!(parse_headers("no-equals-sign").is_none());
        assert!(parse_headers("").unwrap().is_empty());
    }

    #[test]
    fn a_file_error_names_the_setting_where_config_has_it() {
        let error = |upstream: &str| {
            let config = Config::from_toml_str(&format!(
                "[server]\nlisten = \"127.0.0.1:8080\"\n{upstream}\
                 base_url = \"https://api.anthropic.com\"\nprovider_key = {{ file = \"missing.key\" }}\n"
            ))
            .unwrap();
            Resources::resolve(&config, Path::new("/nonexistent-wye-dir"))
                .unwrap_err()
                .to_string()
        };
        let unnamed = error("[[upstreams]]\n");
        assert!(
            unnamed.starts_with(
                "upstreams \"anthropic\": provider_key \
                 /nonexistent-wye-dir/missing.key: "
            ),
            "{unnamed}"
        );
        let entry = error("[[upstreams]]\nid = \"a\"\n");
        assert!(
            entry.starts_with(
                "upstreams \"a\": provider_key \
                 /nonexistent-wye-dir/missing.key: "
            ),
            "{entry}"
        );
    }

    /// Every secret takes either source; a file is read relative to the
    /// config and trimmed, as a provider key's is.
    #[test]
    fn the_hash_key_and_otlp_headers_come_from_files_too() {
        let dir = wye_testkit::TempDir::new("secret-files");
        let config = Config::from_toml_str(
            "[[upstreams]]\nforward_caller_credential = true\n\
             [cache_diagnostics]\nhash_key = { file = \"hash.key\" }\n\
             [telemetry]\notlp_endpoint = \"http://127.0.0.1:4318\"\n\
             otlp_headers = { file = \"otlp.headers\" }\n",
        )
        .unwrap();
        std::fs::write(dir.join("hash.key"), format!("{}\n", "k".repeat(32))).unwrap();
        std::fs::write(dir.join("otlp.headers"), "api-key=secret\n").unwrap();
        let resources = Resources::resolve(&config, &dir).unwrap();
        assert!(resources.prompt_key.is_some());
        assert_eq!(resources.otlp_headers["api-key"], "secret");

        std::fs::write(dir.join("hash.key"), "short\n").unwrap();
        assert_eq!(
            Resources::resolve(&config, &dir).unwrap_err().to_string(),
            "cache_diagnostics: hash_key hash.key must hold at least 32 bytes (for example \
             `openssl rand -hex 32`)"
        );
        std::fs::remove_file(dir.join("hash.key")).unwrap();
        let missing = Resources::resolve(&config, &dir).unwrap_err().to_string();
        assert!(
            missing.starts_with(&format!(
                "cache_diagnostics: hash_key {}: ",
                dir.join("hash.key").display()
            )),
            "{missing}"
        );
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let auth = UpstreamAuth::api_key("sk-secret").unwrap();
        assert_eq!(format!("{auth:?}"), "ApiKey(<redacted>)");
        let auth = UpstreamAuth::bearer("sk-secret").unwrap();
        assert_eq!(format!("{auth:?}"), "Bearer(<redacted>)");
    }
}
