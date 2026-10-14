//! `wye.toml` parsing and validation.
//!
//! Raw TOML is parsed into private `Raw*` types and validated once into public
//! types whose invariants later code relies on. Secrets and files are referenced
//! by environment variable name or path and never read here.

use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use http::{
    Uri,
    uri::{Authority, Scheme},
};
use serde::Deserialize;

use crate::model::ModelPattern;

/// Request bytes held process-wide while reading `model` for permissions.
const DEFAULT_MAX_HELD_REQUEST_BYTES: u64 = 64 * 1024 * 1024;
/// Anthropic's request size limit.
const DEFAULT_MAX_REQUEST_BODY_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 30;
/// Generous for one process: an idle Claude Code stream costs Wye about 5-30 KB.
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 10_000;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
/// Non-streaming generations send no headers until they complete.
const DEFAULT_RESPONSE_HEADER_TIMEOUT_SECS: u64 = 600;
/// Must exceed the provider's SSE `ping` interval during long thinking pauses.
const DEFAULT_STREAM_IDLE_TIMEOUT_SECS: u64 = 120;

/// Validated gateway configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub server: Server,
    pub upstream: Upstream,
    /// Gateway credentials callers must present. Without it, Wye listens on
    /// loopback only.
    pub auth: Option<Auth>,
    /// Prompt hashes in the usage log, to explain prompt-cache misses.
    pub cache_diagnostics: Option<CacheDiagnostics>,
    /// OTLP export of usage records.
    pub telemetry: Option<Telemetry>,
}

/// OTLP export: each usage record is also sent as an OTLP log record.
#[derive(Debug, Clone)]
pub struct Telemetry {
    /// Logs endpoint: the configured base URL with `/v1/logs` appended.
    pub otlp_logs_uri: Uri,
    /// Metrics endpoint: the base URL with `/v1/metrics` appended.
    pub otlp_metrics_uri: Uri,
    /// Environment variable with extra request headers, in the OpenTelemetry
    /// format `key1=value1,key2=value2` (for example a backend API key).
    pub otlp_headers_env: Option<String>,
    /// `service.name` resource attribute.
    pub service_name: String,
}

/// Cache-miss diagnostics (see [`crate::prompt`]).
#[derive(Debug, Clone)]
pub struct CacheDiagnostics {
    /// Environment variable holding the secret that keys the exported hashes
    /// (at least [`crate::prompt::HashKey::MIN_LEN`] bytes). Required: an
    /// unkeyed hash could be matched against guessed prompts.
    pub hash_key_env: String,
    /// Percentage of Claude Code sessions diagnosed, 1-100 (see
    /// [`crate::prompt::session_sampled`]).
    pub sample_percent: u8,
}

/// Signed gateway credentials (see [`crate::credential`]).
#[derive(Debug, Clone)]
pub struct Auth {
    /// Required `iss` claim.
    pub issuer: String,
    /// Required `aud` claim (one of them, if a list).
    pub audience: String,
    /// Longest accepted `exp - iat`: without a revocation list, offboarding
    /// takes effect when the person's last credential expires.
    pub max_credential_ttl: Duration,
    /// Verification keys, selected by the token's `kid`; several allow rotation.
    pub keys: Vec<PublicKey>,
    /// Models any credential may use; `None` allows every model. A
    /// credential's `models` claim narrows this, never widens it.
    pub models: Option<Vec<ModelPattern>>,
}

#[derive(Debug, Clone)]
pub struct PublicKey {
    pub id: String,
    pub ed25519_public_key: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct Server {
    /// Always a loopback address until gateway credentials are implemented.
    pub listen: SocketAddr,
    pub max_request_body_bytes: u64,
    /// Request bytes held across all requests while reading `model` for model
    /// permissions; beyond it, requests are answered with an overload error.
    pub max_held_request_bytes: u64,
    /// Proxied requests in flight (until their response body ends) before new
    /// ones are answered with an overload error instead of queueing.
    pub max_concurrent_requests: usize,
    /// After a shutdown signal, `/readyz` fails but connections are still
    /// accepted for this long, so load balancers stop routing here before
    /// new connections are refused. Zero for local use.
    pub drain_delay: Duration,
    /// After the drain delay, active requests get this long to finish.
    pub shutdown_grace: Duration,
}

#[derive(Debug, Clone)]
pub struct Upstream {
    pub scheme: Scheme,
    pub authority: Authority,
    /// Base path without a trailing slash; empty for a bare origin.
    pub path_prefix: String,
    pub credential: UpstreamCredential,
    /// PEM file of extra root certificates trusted for the upstream, in addition
    /// to the built-in roots. Relative paths are relative to the config file.
    pub ca_file: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub response_header_timeout: Duration,
    pub stream_idle_timeout: Duration,
}

/// How Wye authenticates to the upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamCredential {
    /// Provider API key read from this environment variable at startup.
    ApiKeyEnv(String),
    /// Provider API key read from this file at startup (a mounted secret);
    /// relative to the config file.
    ApiKeyFile(PathBuf),
    /// Forward the caller's own credential (loopback only).
    ForwardCaller,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Parse(String),
    InvalidListen(String),
    NonLoopbackListen(SocketAddr),
    Auth(String),
    InvalidBaseUrl {
        url: String,
        reason: &'static str,
    },
    InvalidUrl {
        field: &'static str,
        url: String,
        reason: &'static str,
    },
    Credential(&'static str),
    InvalidEnvName {
        field: &'static str,
        name: String,
    },
    EmptyPath(&'static str),
    Zero(&'static str),
    Percent(&'static str),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, "invalid configuration: {msg}"),
            Self::InvalidListen(addr) => {
                write!(f, "server.listen: invalid socket address {addr:?}")
            }
            Self::NonLoopbackListen(addr) => write!(
                f,
                "server.listen: {addr} is not a loopback address; network exposure requires an [auth] section"
            ),
            Self::Auth(msg) => write!(f, "auth: {msg}"),
            Self::InvalidBaseUrl { url, reason } => {
                write!(f, "upstream.base_url {url:?}: {reason}")
            }
            Self::InvalidUrl { field, url, reason } => write!(f, "{field} {url:?}: {reason}"),
            Self::Credential(msg) => write!(f, "upstream: {msg}"),
            Self::InvalidEnvName { field, name } => {
                write!(
                    f,
                    "{field}: {name:?} is not a valid environment variable name"
                )
            }
            Self::EmptyPath(field) => write!(f, "{field} must not be empty"),
            Self::Zero(field) => write!(f, "{field} must be greater than zero"),
            Self::Percent(field) => write!(f, "{field} must be between 1 and 100"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: RawServer,
    upstream: RawUpstream,
    auth: Option<RawAuth>,
    cache_diagnostics: Option<RawCacheDiagnostics>,
    telemetry: Option<RawTelemetry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTelemetry {
    otlp_endpoint: String,
    otlp_headers_env: Option<String>,
    service_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCacheDiagnostics {
    hash_key_env: String,
    sample_percent: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    issuer: String,
    audience: String,
    max_credential_ttl_secs: Option<u64>,
    keys: Vec<RawPublicKey>,
    models: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublicKey {
    id: String,
    ed25519_public_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    listen: String,
    max_request_body_bytes: Option<u64>,
    max_held_request_bytes: Option<u64>,
    max_concurrent_requests: Option<usize>,
    drain_delay_secs: Option<u64>,
    shutdown_grace_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUpstream {
    base_url: String,
    api_key_env: Option<String>,
    api_key_file: Option<PathBuf>,
    #[serde(default)]
    forward_caller_credential: bool,
    ca_file: Option<PathBuf>,
    connect_timeout_secs: Option<u64>,
    response_header_timeout_secs: Option<u64>,
    stream_idle_timeout_secs: Option<u64>,
}

impl Config {
    /// Parses and validates a TOML document.
    pub fn from_toml_str(source: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig =
            toml::from_str(source).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let auth = raw.auth.map(validate_auth).transpose()?;
        let upstream = validate_upstream(raw.upstream)?;
        if auth.is_some() && upstream.credential == UpstreamCredential::ForwardCaller {
            return Err(ConfigError::Auth(
                "cannot be combined with upstream.forward_caller_credential: the caller's \
                 credential would be the gateway credential, not a provider credential"
                    .into(),
            ));
        }
        let cache_diagnostics = raw
            .cache_diagnostics
            .map(|raw| {
                let sample_percent = match raw.sample_percent {
                    None => 100,
                    Some(p @ 1..=100) => p,
                    Some(_) => {
                        return Err(ConfigError::Percent("cache_diagnostics.sample_percent"));
                    }
                };
                Ok(CacheDiagnostics {
                    hash_key_env: env_name(raw.hash_key_env, "cache_diagnostics.hash_key_env")?,
                    sample_percent,
                })
            })
            .transpose()?;
        let telemetry = raw.telemetry.map(validate_telemetry).transpose()?;
        Ok(Self {
            server: validate_server(raw.server, auth.is_some())?,
            upstream,
            auth,
            cache_diagnostics,
            telemetry,
        })
    }
}

/// Default `max_credential_ttl_secs`: one week.
const DEFAULT_MAX_CREDENTIAL_TTL_SECS: u64 = 7 * 24 * 3600;

fn validate_auth(raw: RawAuth) -> Result<Auth, ConfigError> {
    let err = |msg: String| ConfigError::Auth(msg);
    if raw.issuer.is_empty() || raw.audience.is_empty() {
        return Err(err("issuer and audience must not be empty".into()));
    }
    if raw.keys.is_empty() {
        return Err(err("at least one key is required".into()));
    }
    let mut keys = Vec::with_capacity(raw.keys.len());
    for key in raw.keys {
        if key.id.is_empty() {
            return Err(err("key id must not be empty".into()));
        }
        if keys.iter().any(|k: &PublicKey| k.id == key.id) {
            return Err(err(format!("duplicate key id {:?}", key.id)));
        }
        let Some(public) = crate::credential::decode_public_key(&key.ed25519_public_key) else {
            return Err(err(format!(
                "key {:?}: ed25519_public_key must be 32 bytes in unpadded base64url",
                key.id
            )));
        };
        keys.push(PublicKey {
            id: key.id,
            ed25519_public_key: public,
        });
    }
    let models = match raw.models {
        Some(list) if list.is_empty() => {
            return Err(err(
                "models must not be empty; omit it to allow every model".into(),
            ));
        }
        Some(list) => Some(
            list.iter()
                .map(|p| {
                    ModelPattern::parse(p).ok_or_else(|| {
                        err(format!(
                            "models: {p:?} is not a model name or a prefix ending in *"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        None => None,
    };
    Ok(Auth {
        issuer: raw.issuer,
        models,
        audience: raw.audience,
        max_credential_ttl: secs(
            raw.max_credential_ttl_secs
                .unwrap_or(DEFAULT_MAX_CREDENTIAL_TTL_SECS),
            "auth.max_credential_ttl_secs",
        )?,
        keys,
    })
}

fn validate_server(raw: RawServer, authenticated: bool) -> Result<Server, ConfigError> {
    let listen: SocketAddr = raw
        .listen
        .parse()
        .map_err(|_| ConfigError::InvalidListen(raw.listen.clone()))?;
    if !listen.ip().is_loopback() && !authenticated {
        return Err(ConfigError::NonLoopbackListen(listen));
    }
    Ok(Server {
        listen,
        max_request_body_bytes: nonzero(
            raw.max_request_body_bytes
                .unwrap_or(DEFAULT_MAX_REQUEST_BODY_BYTES),
            "server.max_request_body_bytes",
        )?,
        max_held_request_bytes: nonzero(
            raw.max_held_request_bytes
                .unwrap_or(DEFAULT_MAX_HELD_REQUEST_BYTES),
            "server.max_held_request_bytes",
        )?,
        max_concurrent_requests: match raw.max_concurrent_requests {
            Some(0) => return Err(ConfigError::Zero("server.max_concurrent_requests")),
            value => value.unwrap_or(DEFAULT_MAX_CONCURRENT_REQUESTS),
        },
        drain_delay: Duration::from_secs(raw.drain_delay_secs.unwrap_or(0)),
        // Zero is valid: cancel active streams immediately on shutdown.
        shutdown_grace: Duration::from_secs(
            raw.shutdown_grace_secs
                .unwrap_or(DEFAULT_SHUTDOWN_GRACE_SECS),
        ),
    })
}

fn validate_upstream(raw: RawUpstream) -> Result<Upstream, ConfigError> {
    let credential = match (
        raw.api_key_env,
        raw.api_key_file,
        raw.forward_caller_credential,
    ) {
        (Some(name), None, false) => {
            UpstreamCredential::ApiKeyEnv(env_name(name, "upstream.api_key_env")?)
        }
        (None, Some(path), false) => {
            if path.as_os_str().is_empty() {
                return Err(ConfigError::EmptyPath("upstream.api_key_file"));
            }
            UpstreamCredential::ApiKeyFile(path)
        }
        (None, None, true) => UpstreamCredential::ForwardCaller,
        (None, None, false) => {
            return Err(ConfigError::Credential(
                "set api_key_env, api_key_file, or forward_caller_credential = true",
            ));
        }
        _ => {
            return Err(ConfigError::Credential(
                "set only one of api_key_env, api_key_file, and forward_caller_credential",
            ));
        }
    };
    let (scheme, authority, path_prefix) = parse_base_url(&raw.base_url)?;
    Ok(Upstream {
        scheme,
        authority,
        path_prefix,
        credential,
        ca_file: match raw.ca_file {
            Some(path) if path.as_os_str().is_empty() => {
                return Err(ConfigError::EmptyPath("upstream.ca_file"));
            }
            path => path,
        },
        connect_timeout: secs(
            raw.connect_timeout_secs
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
            "upstream.connect_timeout_secs",
        )?,
        response_header_timeout: secs(
            raw.response_header_timeout_secs
                .unwrap_or(DEFAULT_RESPONSE_HEADER_TIMEOUT_SECS),
            "upstream.response_header_timeout_secs",
        )?,
        stream_idle_timeout: secs(
            raw.stream_idle_timeout_secs
                .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT_SECS),
            "upstream.stream_idle_timeout_secs",
        )?,
    })
}

fn validate_telemetry(raw: RawTelemetry) -> Result<Telemetry, ConfigError> {
    let invalid = |reason| ConfigError::InvalidUrl {
        field: "telemetry.otlp_endpoint",
        url: raw.otlp_endpoint.clone(),
        reason,
    };
    let uri: Uri = raw
        .otlp_endpoint
        .parse()
        .map_err(|_| invalid("not a valid URL"))?;
    let parts = uri.into_parts();
    let scheme = parts.scheme.ok_or_else(|| invalid("missing scheme"))?;
    if scheme != Scheme::HTTP && scheme != Scheme::HTTPS {
        return Err(invalid("scheme must be http or https"));
    }
    let authority = parts.authority.ok_or_else(|| invalid("missing host"))?;
    let base = match parts.path_and_query {
        Some(pq) if pq.query().is_some() => return Err(invalid("query strings are not allowed")),
        Some(pq) => pq.path().trim_end_matches('/').to_owned(),
        None => String::new(),
    };
    let signal_uri = |signal: &str| {
        Uri::builder()
            .scheme(scheme.clone())
            .authority(authority.clone())
            .path_and_query(format!("{base}/v1/{signal}"))
            .build()
            .map_err(|_| invalid("not a valid URL"))
    };
    let otlp_logs_uri = signal_uri("logs")?;
    let otlp_metrics_uri = signal_uri("metrics")?;
    Ok(Telemetry {
        otlp_logs_uri,
        otlp_metrics_uri,
        otlp_headers_env: raw
            .otlp_headers_env
            .map(|name| env_name(name, "telemetry.otlp_headers_env"))
            .transpose()?,
        service_name: match raw.service_name {
            Some(name) if name.is_empty() => {
                return Err(ConfigError::EmptyPath("telemetry.service_name"));
            }
            name => name.unwrap_or_else(|| "wye".to_owned()),
        },
    })
}

fn parse_base_url(url: &str) -> Result<(Scheme, Authority, String), ConfigError> {
    let invalid = |reason| ConfigError::InvalidBaseUrl {
        url: url.to_owned(),
        reason,
    };
    let uri: Uri = url.parse().map_err(|_| invalid("not a valid URL"))?;
    let parts = uri.into_parts();
    let scheme = parts.scheme.ok_or_else(|| invalid("missing scheme"))?;
    let authority = parts.authority.ok_or_else(|| invalid("missing host"))?;
    if authority.as_str().contains('@') {
        return Err(invalid("credentials in the URL are not allowed"));
    }
    if scheme == Scheme::HTTP {
        if !is_loopback_host(authority.host()) {
            return Err(invalid("plain http is allowed only for loopback hosts"));
        }
    } else if scheme != Scheme::HTTPS {
        return Err(invalid("scheme must be https"));
    }
    let path_prefix = match parts.path_and_query {
        Some(pq) if pq.query().is_some() => return Err(invalid("query strings are not allowed")),
        Some(pq) => pq.path().trim_end_matches('/').to_owned(),
        None => String::new(),
    };
    Ok((scheme, authority, path_prefix))
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn env_name(name: String, field: &'static str) -> Result<String, ConfigError> {
    let mut bytes = name.bytes();
    let valid = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if valid {
        Ok(name)
    } else {
        Err(ConfigError::InvalidEnvName { field, name })
    }
}

fn nonzero(value: u64, field: &'static str) -> Result<u64, ConfigError> {
    if value == 0 {
        Err(ConfigError::Zero(field))
    } else {
        Ok(value)
    }
}

fn secs(value: u64, field: &'static str) -> Result<Duration, ConfigError> {
    nonzero(value, field).map(Duration::from_secs)
}

#[cfg(test)]
mod tests;
