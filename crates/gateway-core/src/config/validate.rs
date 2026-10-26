//! Raw TOML shapes and the validation that turns them into [`Config`] types.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use http::{
    Uri,
    uri::{Authority, Scheme},
};
use serde::Deserialize;

use super::*;

mod auth;
mod models;
mod upstream;

pub(super) use auth::{RawAuth, validate_auth};
pub(super) use models::{
    RawAlias, RawPricing, RawUpgradeCheck, validate_aliases, validate_pricing,
    validate_upgrade_check,
};
pub(super) use upstream::{RawRoutes, RawUpstream, validate_routes};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawConfig {
    pub(super) server: RawServer,
    pub(super) upstream: Option<RawUpstream>,
    pub(super) routes: Option<RawRoutes>,
    pub(super) auth: Option<RawAuth>,
    pub(super) cache_diagnostics: Option<RawCacheDiagnostics>,
    pub(super) telemetry: Option<RawTelemetry>,
    pub(super) discovery: Option<RawDiscovery>,
    #[serde(default)]
    pub(super) aliases: Vec<RawAlias>,
    pub(super) pricing: Option<RawPricing>,
    pub(super) upgrade_check: Option<RawUpgradeCheck>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawDiscovery {
    pub(super) models: Vec<RawDiscoveredModel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawDiscoveredModel {
    pub(super) id: String,
    pub(super) display_name: Option<String>,
    pub(super) description: Option<String>,
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
pub(super) struct RawServer {
    pub(super) listen: String,
    pub(super) max_request_body_bytes: Option<u64>,
    pub(super) max_held_request_bytes: Option<u64>,
    pub(super) max_concurrent_requests: Option<usize>,
    pub(super) drain_delay_secs: Option<u64>,
    pub(super) shutdown_grace_secs: Option<u64>,
    pub(super) client_write_timeout_secs: Option<u64>,
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
        client_write_timeout: secs(
            raw.client_write_timeout_secs
                .unwrap_or(DEFAULT_CLIENT_WRITE_TIMEOUT_SECS),
            "server.client_write_timeout_secs",
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

pub(super) fn validate_discovery(raw: RawDiscovery) -> Result<Discovery, ConfigError> {
    if raw.models.is_empty() {
        return Err(ConfigError::Invalid(
            "discovery.models must not be empty; omit [discovery] to disable GET /v1/models",
        ));
    }
    if raw.models.len() > MAX_DISCOVERY_MODELS {
        return Err(ConfigError::Invalid(
            "discovery.models: at most 1000 models",
        ));
    }
    let mut models: Vec<DiscoveredModel> = Vec::with_capacity(raw.models.len());
    for model in raw.models {
        // A listed id is a name clients send, so it must be one a model
        // permission can match exactly.
        if model.id.contains('*') || ModelPattern::parse(&model.id).is_none() {
            return Err(ConfigError::Invalid(
                "discovery.models: id must be a model name of 1-256 bytes without *",
            ));
        }
        if models.iter().any(|m| m.id == model.id) {
            return Err(ConfigError::Invalid("discovery.models: duplicate id"));
        }
        let too_long = |text: &Option<String>| {
            text.as_ref()
                .is_some_and(|t| t.len() > MAX_DISCOVERY_TEXT_BYTES)
        };
        if too_long(&model.display_name) || too_long(&model.description) {
            return Err(ConfigError::Invalid(
                "discovery.models: display_name and description are at most 1024 bytes",
            ));
        }
        models.push(DiscoveredModel {
            id: model.id,
            display_name: model.display_name,
            description: model.description,
        });
    }
    Ok(Discovery { models })
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
