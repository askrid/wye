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
}

#[derive(Debug, Clone)]
pub struct Server {
    /// Always a loopback address until gateway credentials are implemented.
    pub listen: SocketAddr,
    pub max_request_body_bytes: u64,
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
    /// Forward the caller's own credential (loopback only).
    ForwardCaller,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Parse(String),
    InvalidListen(String),
    NonLoopbackListen(SocketAddr),
    InvalidBaseUrl { url: String, reason: &'static str },
    Credential(&'static str),
    InvalidEnvName(String),
    EmptyPath(&'static str),
    Zero(&'static str),
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
                "server.listen: {addr} is not a loopback address; network exposure requires gateway credentials, which are not implemented yet"
            ),
            Self::InvalidBaseUrl { url, reason } => {
                write!(f, "upstream.base_url {url:?}: {reason}")
            }
            Self::Credential(msg) => write!(f, "upstream: {msg}"),
            Self::InvalidEnvName(name) => {
                write!(
                    f,
                    "upstream.api_key_env: {name:?} is not a valid environment variable name"
                )
            }
            Self::EmptyPath(field) => write!(f, "{field} must not be empty"),
            Self::Zero(field) => write!(f, "{field} must be greater than zero"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: RawServer,
    upstream: RawUpstream,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    listen: String,
    max_request_body_bytes: Option<u64>,
    max_concurrent_requests: Option<usize>,
    drain_delay_secs: Option<u64>,
    shutdown_grace_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUpstream {
    base_url: String,
    api_key_env: Option<String>,
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
        Ok(Self {
            server: validate_server(raw.server)?,
            upstream: validate_upstream(raw.upstream)?,
        })
    }
}

fn validate_server(raw: RawServer) -> Result<Server, ConfigError> {
    let listen: SocketAddr = raw
        .listen
        .parse()
        .map_err(|_| ConfigError::InvalidListen(raw.listen.clone()))?;
    if !listen.ip().is_loopback() {
        return Err(ConfigError::NonLoopbackListen(listen));
    }
    Ok(Server {
        listen,
        max_request_body_bytes: nonzero(
            raw.max_request_body_bytes
                .unwrap_or(DEFAULT_MAX_REQUEST_BODY_BYTES),
            "server.max_request_body_bytes",
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
    let credential = match (raw.api_key_env, raw.forward_caller_credential) {
        (Some(name), false) => {
            if !is_env_name(&name) {
                return Err(ConfigError::InvalidEnvName(name));
            }
            UpstreamCredential::ApiKeyEnv(name)
        }
        (None, true) => UpstreamCredential::ForwardCaller,
        (Some(_), true) => {
            return Err(ConfigError::Credential(
                "set either api_key_env or forward_caller_credential, not both",
            ));
        }
        (None, false) => {
            return Err(ConfigError::Credential(
                "set api_key_env, or forward_caller_credential = true",
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

fn is_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
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
mod tests {
    use super::*;

    fn parse(upstream: &str) -> Result<Config, ConfigError> {
        Config::from_toml_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n[upstream]\n{upstream}"
        ))
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let config =
            parse("base_url = \"https://api.anthropic.com\"\napi_key_env = \"ANTHROPIC_API_KEY\"")
                .unwrap();
        assert_eq!(
            config.server.max_request_body_bytes,
            DEFAULT_MAX_REQUEST_BODY_BYTES
        );
        assert_eq!(config.upstream.scheme, Scheme::HTTPS);
        assert_eq!(config.upstream.authority.as_str(), "api.anthropic.com");
        assert_eq!(config.upstream.path_prefix, "");
        assert_eq!(
            config.upstream.credential,
            UpstreamCredential::ApiKeyEnv("ANTHROPIC_API_KEY".into())
        );
        assert_eq!(
            config.upstream.response_header_timeout,
            Duration::from_secs(600)
        );
    }

    #[test]
    fn base_path_keeps_prefix_without_trailing_slash() {
        let config = parse(
            "base_url = \"https://gw.example.com/anthropic/\"\nforward_caller_credential = true",
        )
        .unwrap();
        assert_eq!(config.upstream.path_prefix, "/anthropic");
        assert_eq!(
            config.upstream.credential,
            UpstreamCredential::ForwardCaller
        );
    }

    #[test]
    fn rejects_non_loopback_listen() {
        let err = Config::from_toml_str(
            "[server]\nlisten = \"0.0.0.0:8080\"\n[upstream]\nbase_url = \"https://api.anthropic.com\"\nforward_caller_credential = true",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::NonLoopbackListen(_)));
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = parse("base_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\napi_key = \"sk-secret\"").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn credential_must_be_exactly_one() {
        assert!(matches!(
            parse("base_url = \"https://api.anthropic.com\""),
            Err(ConfigError::Credential(_))
        ));
        assert!(matches!(
            parse(
                "base_url = \"https://api.anthropic.com\"\napi_key_env = \"K\"\nforward_caller_credential = true"
            ),
            Err(ConfigError::Credential(_))
        ));
        assert!(matches!(
            parse("base_url = \"https://api.anthropic.com\"\napi_key_env = \"1BAD\""),
            Err(ConfigError::InvalidEnvName(_))
        ));
    }

    #[test]
    fn base_url_rules() {
        for (url, ok) in [
            ("http://127.0.0.1:9000", true),
            ("http://localhost:9000", true),
            ("http://[::1]:9000", true),
            ("http://api.anthropic.com", false),
            ("ftp://api.anthropic.com", false),
            ("https://api.anthropic.com/?x=1", false),
            ("https://user:pw@api.anthropic.com", false),
            ("/relative", false),
        ] {
            let result = parse(&format!(
                "base_url = \"{url}\"\nforward_caller_credential = true"
            ));
            assert_eq!(result.is_ok(), ok, "{url}: {result:?}");
        }
    }

    #[test]
    fn ca_file_is_kept_as_written() {
        let ok = "base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\n";
        let config = parse(&format!("{ok}ca_file = \"certs/ca.pem\"")).unwrap();
        assert_eq!(config.upstream.ca_file, Some(PathBuf::from("certs/ca.pem")));
        assert_eq!(parse(ok).unwrap().upstream.ca_file, None);
        assert_eq!(
            parse(&format!("{ok}ca_file = \"\"")).unwrap_err(),
            ConfigError::EmptyPath("upstream.ca_file")
        );
    }

    #[test]
    fn lifecycle_defaults_and_zero_concurrency_rejected() {
        let config =
            parse("base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true")
                .unwrap();
        assert_eq!(config.server.max_concurrent_requests, 10_000);
        assert_eq!(config.server.drain_delay, Duration::ZERO);
        let err = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:8080\"\nmax_concurrent_requests = 0\n\
             [upstream]\nbase_url = \"https://api.anthropic.com\"\nforward_caller_credential = true",
        )
        .unwrap_err();
        assert_eq!(err, ConfigError::Zero("server.max_concurrent_requests"));
    }

    #[test]
    fn rejects_zero_timeouts() {
        assert_eq!(
            parse("base_url = \"https://api.anthropic.com\"\nforward_caller_credential = true\nstream_idle_timeout_secs = 0")
                .unwrap_err(),
            ConfigError::Zero("upstream.stream_idle_timeout_secs")
        );
    }
}
