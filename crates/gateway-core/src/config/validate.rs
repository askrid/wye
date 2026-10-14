//! Raw TOML shapes and the validation that turns them into [`Config`] types.

use std::{net::IpAddr, net::SocketAddr, path::PathBuf, time::Duration};

use http::{
    Uri,
    uri::{Authority, Scheme},
};
use serde::Deserialize;

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawConfig {
    pub(super) server: RawServer,
    pub(super) upstream: RawUpstream,
    pub(super) auth: Option<RawAuth>,
    pub(super) cache_diagnostics: Option<RawCacheDiagnostics>,
    pub(super) telemetry: Option<RawTelemetry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTelemetry {
    pub(super) otlp_endpoint: String,
    pub(super) otlp_headers_env: Option<String>,
    pub(super) service_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawCacheDiagnostics {
    pub(super) hash_key_env: String,
    pub(super) sample_percent: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawAuth {
    pub(super) issuer: String,
    pub(super) audience: String,
    pub(super) max_credential_ttl_secs: Option<u64>,
    pub(super) keys: Vec<RawPublicKey>,
    pub(super) models: Option<Vec<String>>,
    pub(super) max_output_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPublicKey {
    pub(super) id: String,
    pub(super) ed25519_public_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawServer {
    pub(super) listen: String,
    pub(super) max_request_body_bytes: Option<u64>,
    pub(super) max_held_request_bytes: Option<u64>,
    pub(super) max_concurrent_requests: Option<usize>,
    pub(super) drain_delay_secs: Option<u64>,
    pub(super) shutdown_grace_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawUpstream {
    pub(super) base_url: String,
    pub(super) api_key_env: Option<String>,
    pub(super) api_key_file: Option<PathBuf>,
    #[serde(default)]
    pub(super) forward_caller_credential: bool,
    pub(super) ca_file: Option<PathBuf>,
    pub(super) connect_timeout_secs: Option<u64>,
    pub(super) response_header_timeout_secs: Option<u64>,
    pub(super) stream_idle_timeout_secs: Option<u64>,
}

/// Default `max_credential_ttl_secs`: one week.
const DEFAULT_MAX_CREDENTIAL_TTL_SECS: u64 = 7 * 24 * 3600;

pub(super) fn validate_auth(raw: RawAuth) -> Result<Auth, ConfigError> {
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
        max_output_tokens: raw
            .max_output_tokens
            .map(|n| nonzero(n, "auth.max_output_tokens"))
            .transpose()?,
        audience: raw.audience,
        max_credential_ttl: secs(
            raw.max_credential_ttl_secs
                .unwrap_or(DEFAULT_MAX_CREDENTIAL_TTL_SECS),
            "auth.max_credential_ttl_secs",
        )?,
        keys,
    })
}

pub(super) fn validate_server(raw: RawServer, authenticated: bool) -> Result<Server, ConfigError> {
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

pub(super) fn validate_upstream(raw: RawUpstream) -> Result<Upstream, ConfigError> {
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

pub(super) fn validate_telemetry(raw: RawTelemetry) -> Result<Telemetry, ConfigError> {
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
    let otlp_traces_uri = signal_uri("traces")?;
    Ok(Telemetry {
        otlp_logs_uri,
        otlp_metrics_uri,
        otlp_traces_uri,
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

pub(super) fn env_name(name: String, field: &'static str) -> Result<String, ConfigError> {
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
