//! Resolves what the configuration references: secrets from the environment
//! and files from disk. Runs once, before serving.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use gateway_core::{
    config::{Config, Issuer, UpstreamCredential},
    credential::{KeySetError, ProviderKeys, Verifier},
    prompt::HashKey,
};
use rustls::pki_types::{CertificateDer, pem::PemObject};

use http::{HeaderMap, HeaderValue};

/// Failures that prevent the gateway from starting.
#[derive(Debug)]
pub enum StartupError {
    MissingEnv(String),
    InvalidApiKey,
    HashKeyTooShort(String),
    KeyFile {
        path: PathBuf,
        reason: String,
    },
    InvalidOtlpHeaders(String),
    CaFile {
        path: PathBuf,
        reason: String,
    },
    JwksFile {
        path: PathBuf,
        reason: String,
    },
    KeySet(KeySetError),
    Tls(String),
    BodyLimitTooLarge,
    /// The resources were not resolved from this configuration.
    UpstreamCount,
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv(name) => write!(
                f,
                "environment variable {name} is not set or not valid UTF-8"
            ),
            Self::InvalidApiKey => {
                f.write_str("upstream API key contains characters not allowed in an HTTP header")
            }
            Self::HashKeyTooShort(name) => write!(
                f,
                "environment variable {name} must hold at least {} bytes (for example `openssl rand -hex 32`)",
                HashKey::MIN_LEN
            ),
            Self::InvalidOtlpHeaders(name) => write!(
                f,
                "environment variable {name} must hold key=value pairs separated by commas"
            ),
            Self::KeyFile { path, reason } => {
                write!(f, "upstream.api_key_file {}: {reason}", path.display())
            }
            Self::CaFile { path, reason } => {
                write!(f, "upstream.ca_file {}: {reason}", path.display())
            }
            Self::JwksFile { path, reason } => {
                write!(f, "auth.issuers jwks_file {}: {reason}", path.display())
            }
            Self::KeySet(e) => write!(f, "auth: {e}"),
            Self::Tls(msg) => write!(f, "TLS setup failed: {msg}"),
            Self::BodyLimitTooLarge => {
                f.write_str("server.max_request_body_bytes exceeds this platform's address space")
            }
            Self::UpstreamCount => {
                f.write_str("one set of upstream resources is needed per configured upstream")
            }
        }
    }
}

impl std::error::Error for StartupError {}

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
    /// Extra headers for OTLP export requests, from `telemetry.otlp_headers_env`.
    pub otlp_headers: HeaderMap,
}

/// What one upstream's configuration references.
#[derive(Debug)]
pub struct UpstreamResources {
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
            let key = match &upstream.credential {
                UpstreamCredential::ApiKeyEnv(name) => Some(env(name)?),
                UpstreamCredential::ApiKeyFile(path) => Some(read_secret(&config_dir.join(path))?),
                UpstreamCredential::ForwardCaller => None,
            };
            let auth = match key {
                Some(key) if upstream.provider.bearer_key() => UpstreamAuth::bearer(&key)?,
                Some(key) => UpstreamAuth::api_key(&key)?,
                None => UpstreamAuth::ForwardCaller,
            };
            let extra_roots = match &upstream.ca_file {
                Some(path) => load_certificates(&config_dir.join(path))?,
                None => Vec::new(),
            };
            upstreams.push(UpstreamResources { auth, extra_roots });
        }
        let prompt_key = match &config.cache_diagnostics {
            Some(diagnostics) => {
                let name = &diagnostics.hash_key_env;
                let key = HashKey::new(env(name)?.as_bytes())
                    .ok_or_else(|| StartupError::HashKeyTooShort(name.clone()))?;
                Some(key)
            }
            None => None,
        };
        let otlp_headers = match config
            .telemetry
            .as_ref()
            .and_then(|t| t.otlp_headers_env.as_ref())
        {
            Some(name) => parse_headers(&env(name)?)
                .ok_or_else(|| StartupError::InvalidOtlpHeaders(name.clone()))?,
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
        .issuers
        .iter()
        .map(|issuer| key_set(issuer, config_dir))
        .collect::<Result<_, _>>()?;
    Verifier::new(auth, sets)
        .map(Some)
        .map_err(StartupError::KeySet)
}

/// An identity provider's key set, read from its `jwks_file` (relative to
/// `config_dir`).
pub(crate) fn key_set(issuer: &Issuer, config_dir: &Path) -> Result<ProviderKeys, StartupError> {
    let path = config_dir.join(&issuer.jwks_file);
    let error = |reason: String| StartupError::JwksFile {
        path: path.clone(),
        reason,
    };
    let jwks = std::fs::read(&path).map_err(|e| error(e.to_string()))?;
    ProviderKeys::parse(&jwks, issuer.algorithm).map_err(|e| error(e.to_string()))
}

/// A secret file's contents without surrounding whitespace (mounted secrets
/// often end in a newline).
pub(crate) fn read_secret(path: &Path) -> Result<String, StartupError> {
    let error = |reason: String| StartupError::KeyFile {
        path: path.to_owned(),
        reason,
    };
    let contents = std::fs::read_to_string(path).map_err(|e| error(e.to_string()))?;
    let secret = contents.trim();
    if secret.is_empty() {
        return Err(error("file is empty".into()));
    }
    Ok(secret.to_owned())
}

pub(crate) fn env(name: &str) -> Result<String, StartupError> {
    std::env::var(name).map_err(|_| StartupError::MissingEnv(name.to_owned()))
}

pub(crate) fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, StartupError> {
    let error = |reason: String| StartupError::CaFile {
        path: path.to_owned(),
        reason,
    };
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| error(e.to_string()))?;
    if certs.is_empty() {
        return Err(error("no PEM certificates found".into()));
    }
    Ok(certs)
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
    pub fn api_key(key: &str) -> Result<Self, StartupError> {
        Ok(Self::ApiKey(secret(key)?))
    }

    pub fn bearer(key: &str) -> Result<Self, StartupError> {
        Ok(Self::Bearer(secret(&format!("Bearer {key}"))?))
    }
}

fn secret(value: &str) -> Result<HeaderValue, StartupError> {
    let mut value = HeaderValue::from_str(value).map_err(|_| StartupError::InvalidApiKey)?;
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
pub(crate) fn parse_headers(spec: &str) -> Option<HeaderMap> {
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
}
