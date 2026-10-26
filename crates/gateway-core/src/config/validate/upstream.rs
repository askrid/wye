//! The upstreams: `[upstream]`, or `[routes.claude]` with several.

use std::net::IpAddr;

use serde::Deserialize;

use super::*;
use crate::route::{MAX_UPSTREAMS, MAX_WEIGHT};

/// `[routes]`: one route per native API; Claude's for now.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRoutes {
    pub(crate) claude: RawRoute,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRoute {
    pub(crate) select: Option<String>,
    pub(crate) upstreams: Vec<RawUpstream>,
}

/// `[upstream]`, or one `[[routes.claude.upstreams]]` entry, which also
/// takes `id`, `provider`, and `weight`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawUpstream {
    pub(crate) id: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) weight: Option<u32>,
    pub(crate) base_url: String,
    pub(crate) api_key_env: Option<String>,
    pub(crate) api_key_file: Option<PathBuf>,
    #[serde(default)]
    pub(crate) forward_caller_credential: bool,
    pub(crate) ca_file: Option<PathBuf>,
    pub(crate) connect_timeout_secs: Option<u64>,
    pub(crate) response_header_timeout_secs: Option<u64>,
    pub(crate) stream_idle_timeout_secs: Option<u64>,
    pub(crate) http_version: Option<String>,
}

/// `[upstream]` alone, or `[routes.claude]` alone.
pub(crate) fn validate_route(
    single: Option<RawUpstream>,
    routes: Option<RawRoutes>,
) -> Result<Route, ConfigError> {
    let upstreams = match (single, routes) {
        (Some(raw), None) => {
            if raw.id.is_some() || raw.provider.is_some() || raw.weight.is_some() {
                return Err(ConfigError::Invalid(
                    "upstream: id, provider, and weight belong in [[routes.claude.upstreams]]",
                ));
            }
            vec![validate_upstream(raw)?]
        }
        (None, Some(RawRoutes { claude })) => validate_route_upstreams(claude)?,
        (None, None) => {
            return Err(ConfigError::Invalid(
                "set [upstream], or [routes.claude] for several upstreams",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(ConfigError::Invalid(
                "set either [upstream] or [routes.claude], not both",
            ));
        }
    };
    Ok(Route::new(upstreams))
}

fn validate_route_upstreams(raw: RawRoute) -> Result<Vec<Upstream>, ConfigError> {
    match raw.select.as_deref() {
        None | Some("cache_affine") => {}
        Some(_) => {
            return Err(ConfigError::Invalid(
                "routes.claude.select must be \"cache_affine\"",
            ));
        }
    }
    if raw.upstreams.is_empty() || raw.upstreams.len() > MAX_UPSTREAMS {
        return Err(ConfigError::Invalid(
            "routes.claude.upstreams must have between 1 and 16 entries",
        ));
    }
    let mut upstreams: Vec<Upstream> = Vec::with_capacity(raw.upstreams.len());
    for mut entry in raw.upstreams {
        let Some(id) = entry.id.take() else {
            return Err(ConfigError::Invalid(
                "routes.claude.upstreams: every entry needs an id",
            ));
        };
        let in_entry = |error| ConfigError::RouteUpstream {
            id: id.clone(),
            error: Box::new(error),
        };
        if !is_upstream_id(&id) {
            return Err(in_entry(ConfigError::Invalid(
                "id must be 1-64 letters, digits, '.', '_', or '-'",
            )));
        }
        if upstreams.iter().any(|u| u.id.as_deref() == Some(&*id)) {
            return Err(in_entry(ConfigError::Invalid("id is used twice")));
        }
        match entry.provider.as_deref() {
            None | Some("anthropic") => {}
            Some(_) => {
                return Err(in_entry(ConfigError::Invalid(
                    "provider must be \"anthropic\" (other Claude providers are planned)",
                )));
            }
        }
        let weight = match entry.weight {
            None => 1,
            Some(w @ 1..=MAX_WEIGHT) => w,
            Some(_) => {
                return Err(in_entry(ConfigError::Invalid(
                    "weight must be between 1 and 1000",
                )));
            }
        };
        if entry.forward_caller_credential {
            return Err(in_entry(ConfigError::Credential(
                "forward_caller_credential needs a single [upstream]: a route's upstreams use \
                 their own provider credentials",
            )));
        }
        let mut upstream = validate_upstream(entry).map_err(in_entry)?;
        upstream.id = Some(id.into());
        upstream.weight = weight;
        upstreams.push(upstream);
    }
    Ok(upstreams)
}

/// Ids appear in telemetry attributes: short, printable, no separators.
fn is_upstream_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
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
        id: None,
        weight: 1,
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
        http_version: match raw.http_version.as_deref() {
            None | Some("auto") => UpstreamHttpVersion::Auto,
            Some("1.1") => UpstreamHttpVersion::Http1,
            Some(_) => {
                return Err(ConfigError::Invalid(
                    "upstream.http_version must be \"auto\" or \"1.1\"",
                ));
            }
        },
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
