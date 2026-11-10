//! Raw TOML shapes and the validation that turns them into [`Config`] types.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use http::{
    Uri,
    uri::{Authority, Scheme},
};
use serde::Deserialize;

use super::*;
use crate::policy;

mod auth;
mod models;
mod upstream;

pub(super) use auth::{RawAuth, validate_auth};
pub(super) use models::{
    RawAlias, RawPricing, RawUpgradeCheck, validate_aliases, validate_pricing,
    validate_upgrade_check,
};
pub(super) use upstream::{RawUpstream, validate_routes};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawConfig {
    #[serde(default)]
    pub(super) server: RawServer,
    #[serde(default)]
    pub(super) upstreams: Vec<RawUpstream>,
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
    pub(super) otlp_headers: Option<RawSecret>,
    pub(super) service_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawCacheDiagnostics {
    pub(super) hash_key: RawSecret,
    pub(super) sample_percent: Option<u8>,
}

/// A secret's source: `{ env = "NAME" }` or `{ file = "path" }`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawSecret {
    pub(super) env: Option<String>,
    pub(super) file: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct RawServer {
    pub(super) listen: Option<String>,
    pub(super) max_request_body_bytes: Option<u64>,
    pub(super) max_concurrent_requests: Option<usize>,
    pub(super) drain_delay: Option<String>,
    pub(super) shutdown_grace: Option<String>,
}

pub(super) fn validate_server(raw: RawServer, authenticated: bool) -> Result<Server, ConfigError> {
    let listen = raw.listen.as_deref().unwrap_or(DEFAULT_LISTEN);
    let listen: SocketAddr = listen
        .parse()
        .map_err(|_| ConfigError::InvalidListen(listen.to_owned()))?;
    if !listen.ip().is_loopback() && !authenticated {
        return Err(ConfigError::NonLoopbackListen(listen));
    }
    let max_concurrent_requests = match raw.max_concurrent_requests {
        Some(0) => return Err(ConfigError::Zero("server.max_concurrent_requests")),
        value => value.unwrap_or(DEFAULT_MAX_CONCURRENT_REQUESTS),
    };
    Ok(Server {
        listen,
        max_request_body_bytes: nonzero(
            raw.max_request_body_bytes
                .unwrap_or(DEFAULT_MAX_REQUEST_BODY_BYTES),
            "server.max_request_body_bytes",
        )?,
        max_concurrent_requests,
        drain_delay: duration(
            raw.drain_delay,
            DEFAULT_DRAIN_DELAY,
            MAX_TIMER,
            "server.drain_delay",
        )?,
        // Zero is valid: cancel active streams immediately on shutdown.
        shutdown_grace: duration(
            raw.shutdown_grace,
            DEFAULT_SHUTDOWN_GRACE,
            MAX_TIMER,
            "server.shutdown_grace",
        )?,
    })
}

/// A secret named by exactly one source: a valid environment variable name,
/// or a file path. `field` names it in errors.
pub(super) fn validate_secret(raw: RawSecret, field: &'static str) -> Result<Secret, ConfigError> {
    match (raw.env, raw.file) {
        (Some(name), None) => env_name(name, field).map(Secret::Env),
        (None, Some(path)) if path.as_os_str().is_empty() => Err(ConfigError::Empty(field)),
        (None, Some(path)) => Ok(Secret::File(path)),
        _ => Err(ConfigError::Secret(field)),
    }
}

/// An https URL, or an http one to a loopback host, without credentials or
/// a query: its scheme, host, and path without a trailing slash. Secrets
/// come from a [`Secret`], never from a URL in config, and what a
/// request carries (a key, OTLP headers, prompts) never crosses a network
/// in the clear. `field` names it in errors.
pub(super) fn parse_url(
    field: &'static str,
    url: &str,
) -> Result<(Scheme, Authority, String), ConfigError> {
    let invalid = |reason| ConfigError::InvalidUrl {
        field,
        url: url.to_owned(),
        reason,
    };
    let parts = url
        .parse::<Uri>()
        .map_err(|_| invalid("not a valid URL"))?
        .into_parts();
    let scheme = parts.scheme.ok_or_else(|| invalid("missing scheme"))?;
    if scheme != Scheme::HTTP && scheme != Scheme::HTTPS {
        return Err(invalid("scheme must be http or https"));
    }
    let authority = parts.authority.ok_or_else(|| invalid("missing host"))?;
    if authority.as_str().contains('@') {
        return Err(invalid("credentials in the URL are not allowed"));
    }
    if scheme == Scheme::HTTP && !is_loopback_host(authority.host()) {
        return Err(invalid("plain http is allowed only for loopback hosts"));
    }
    let path = match parts.path_and_query {
        Some(pq) if pq.query().is_some() => return Err(invalid("query strings are not allowed")),
        Some(pq) => pq.path().trim_end_matches('/').to_owned(),
        None => String::new(),
    };
    Ok((scheme, authority, path))
}

pub(super) fn validate_telemetry(raw: RawTelemetry) -> Result<Telemetry, ConfigError> {
    const FIELD: &str = "telemetry.otlp_endpoint";
    let invalid = |reason| ConfigError::InvalidUrl {
        field: FIELD,
        url: raw.otlp_endpoint.clone(),
        reason,
    };
    let (scheme, authority, base) = parse_url(FIELD, &raw.otlp_endpoint)?;
    let signal_uri = |signal: &str| {
        Uri::builder()
            .scheme(scheme.clone())
            .authority(authority.clone())
            .path_and_query(format!("{base}/v1/{signal}"))
            .build()
            .map_err(|_| invalid("not a valid URL"))
    };
    Ok(Telemetry {
        otlp_logs_uri: signal_uri("logs")?,
        otlp_metrics_uri: signal_uri("metrics")?,
        otlp_traces_uri: signal_uri("traces")?,
        otlp_headers: raw
            .otlp_headers
            .map(|raw| validate_secret(raw, "telemetry.otlp_headers"))
            .transpose()?,
        service_name: match raw.service_name {
            Some(name) if name.is_empty() => {
                return Err(ConfigError::Empty("telemetry.service_name"));
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
        if !policy::is_model_name(&model.id) {
            return Err(ConfigError::Invalid(
                "discovery.models: id must be a model name of 1-256 bytes without * or escapes",
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

/// A sampling share of 1-100, by default [`DEFAULT_SAMPLE_PERCENT`].
pub(super) fn sample_percent(value: Option<u8>, field: &'static str) -> Result<u8, ConfigError> {
    match value {
        None => Ok(DEFAULT_SAMPLE_PERCENT),
        Some(p @ 1..=100) => Ok(p),
        Some(_) => Err(ConfigError::Percent(field)),
    }
}

/// The longest timer, a day: longer ones are mistakes, and a timer adds its
/// duration to an `Instant`, which must not overflow.
const MAX_TIMER: Duration = Duration::from_secs(24 * 3600);

/// A duration as written, else `default`; at most `max`, zero allowed.
fn duration(
    raw: Option<String>,
    default: Duration,
    max: Duration,
    field: &'static str,
) -> Result<Duration, ConfigError> {
    let value = match raw {
        None => default,
        Some(text) => crate::duration::parse(&text).ok_or(ConfigError::Duration(field))?,
    };
    if value > max {
        return Err(ConfigError::TooLong { field, max });
    }
    Ok(value)
}

/// A duration that must run: more than zero, at most `max`.
fn running(
    raw: Option<String>,
    default: Duration,
    max: Duration,
    field: &'static str,
) -> Result<Duration, ConfigError> {
    match duration(raw, default, max, field)? {
        Duration::ZERO => Err(ConfigError::Zero(field)),
        value => Ok(value),
    }
}
