//! `wye.toml` parsing and validation.
//!
//! Raw TOML is parsed into private `Raw*` types and validated once into public
//! types whose invariants later code relies on. Secrets and files are referenced
//! by environment variable name or path and never read here.

use std::{fmt, net::SocketAddr, path::PathBuf, time::Duration};

use http::{
    Uri,
    uri::{Authority, Scheme},
};

mod validate;

use crate::policy::ModelPattern;
use validate::{
    RawConfig, env_name, validate_auth, validate_server, validate_telemetry, validate_upstream,
};

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
    /// Traces endpoint: the base URL with `/v1/traces` appended.
    pub otlp_traces_uri: Uri,
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
    /// Largest `max_tokens` any credential may request on `/v1/messages`;
    /// a credential's `max_output_tokens` claim can only lower it.
    pub max_output_tokens: Option<u64>,
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
    /// Protocols offered to a TLS upstream.
    pub http_version: UpstreamHttpVersion,
}

/// Protocols Wye offers a TLS upstream through ALPN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamHttpVersion {
    /// HTTP/2 when the upstream offers it, else HTTP/1.1 (the default).
    Auto,
    /// HTTP/1.1 only: no HTTP/2 flow control on uploads, one connection
    /// per concurrent request.
    Http1,
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
    Invalid(&'static str),
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
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

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

#[cfg(test)]
mod tests;
