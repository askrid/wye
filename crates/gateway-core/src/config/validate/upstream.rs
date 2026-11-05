//! The routes and their upstreams: `[routes.claude]` for Anthropic
//! Messages, and `[routes.openai]` for OpenAI Responses and Chat Completions.

use serde::Deserialize;

use super::*;
use crate::route::{Failover, FailoverReason, MAX_UPSTREAMS, MAX_WEIGHT, Select};

/// `[routes]`: one route per native API.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRoutes {
    pub(crate) claude: Option<RawRoute>,
    pub(crate) openai: Option<RawRoute>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRoute {
    pub(crate) select: Option<String>,
    pub(crate) failover: Option<Vec<String>>,
    pub(crate) failover_max_body_bytes: Option<u64>,
    pub(crate) upstreams: Vec<RawUpstream>,
}

/// One `[[routes.<route>.upstreams]]` entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawUpstream {
    pub(crate) id: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) weight: Option<u32>,
    pub(crate) workspace_id: Option<String>,
    pub(crate) base_url: String,
    pub(crate) provider_key_env: Option<String>,
    pub(crate) provider_key_file: Option<PathBuf>,
    #[serde(default)]
    pub(crate) forward_caller_credential: bool,
    pub(crate) ca_file: Option<PathBuf>,
    pub(crate) connect_timeout_secs: Option<u64>,
    pub(crate) response_header_timeout_secs: Option<u64>,
    pub(crate) stream_idle_timeout_secs: Option<u64>,
    pub(crate) http_version: Option<String>,
}

/// The Claude route from `[routes.claude]` and the OpenAI route from
/// `[routes.openai]`; at least one.
pub(crate) fn validate_routes(routes: Option<RawRoutes>) -> Result<Routes, ConfigError> {
    let (claude, openai) = routes.map_or((None, None), |r| (r.claude, r.openai));
    let claude = claude
        .map(|route| validate_route(route, RouteId::Claude))
        .transpose()?;
    let openai = openai
        .map(|route| validate_route(route, RouteId::OpenAi))
        .transpose()?;
    if claude.is_none() && openai.is_none() {
        return Err(ConfigError::Invalid(
            "set [[routes.claude.upstreams]] for Claude, [[routes.openai.upstreams]] for \
             OpenAI, or both",
        ));
    }
    // Ids name an upstream in usage records and price entries, across routes.
    let ids = |route: &Option<Route>| -> Vec<Box<str>> {
        route
            .iter()
            .flat_map(|r| r.upstreams())
            .filter_map(|u| u.id.clone())
            .collect()
    };
    let claude_ids = ids(&claude);
    if let Some(id) = ids(&openai).into_iter().find(|id| claude_ids.contains(id)) {
        return Err(ConfigError::RouteUpstream {
            route: RouteId::OpenAi.name(),
            id: Some(id.into()),
            error: Box::new(ConfigError::Invalid("id is used by routes.claude too")),
        });
    }
    Ok(Routes { claude, openai })
}

/// One `[routes.<route>]`: its upstreams, selection, and failover.
fn validate_route(raw: RawRoute, id: RouteId) -> Result<Route, ConfigError> {
    let in_route = in_route(id);
    let select = match raw.select.as_deref() {
        None | Some("cache_affine") => Select::CacheAffine,
        Some("ordered") => Select::Ordered,
        Some(_) => {
            return Err(in_route(ConfigError::Invalid(
                "select must be \"cache_affine\" or \"ordered\"",
            )));
        }
    };
    let failover = match raw.failover {
        None => {
            if raw.failover_max_body_bytes.is_some() {
                return Err(in_route(ConfigError::Invalid(
                    "failover_max_body_bytes needs failover",
                )));
            }
            None
        }
        Some(on) => {
            let mut failover = Failover {
                on: 0,
                max_body_bytes: nonzero(
                    raw.failover_max_body_bytes
                        .unwrap_or(DEFAULT_FAILOVER_MAX_BODY_BYTES),
                    "failover_max_body_bytes",
                )
                .map_err(in_route)?,
            };
            for reason in &on {
                // Only rejections that prove the request was not processed:
                // an ambiguous 5xx may already be billed.
                let Some(reason) = FailoverReason::parse(reason) else {
                    return Err(in_route(ConfigError::Invalid(
                        "failover takes \"429\", \"529\", and \"connect\" only: other \
                         failures may already have been processed and billed",
                    )));
                };
                failover.enable(reason);
            }
            if on.is_empty() {
                return Err(in_route(ConfigError::Invalid(
                    "failover must name at least one reason",
                )));
            }
            if raw.upstreams.len() < 2 {
                return Err(in_route(ConfigError::Invalid(
                    "failover needs at least two upstreams",
                )));
            }
            Some(failover)
        }
    };
    let upstreams = validate_route_upstreams(raw.upstreams, id)?;
    Ok(Route::new(upstreams, select, failover))
}

/// Puts an error about one of route `id`'s own fields under its name.
fn in_route(id: RouteId) -> impl Fn(ConfigError) -> ConfigError + Copy {
    move |error| ConfigError::Route {
        route: id.name(),
        error: Box::new(error),
    }
}

fn validate_route_upstreams(
    raw_upstreams: Vec<RawUpstream>,
    id: RouteId,
) -> Result<Vec<Upstream>, ConfigError> {
    // The provider an upstream has unless it names one.
    let (route, provider) = (id.name(), Provider::of_route(id));
    if raw_upstreams.is_empty() || raw_upstreams.len() > MAX_UPSTREAMS {
        return Err(in_route(id)(ConfigError::Invalid(
            "upstreams must have between 1 and 16 entries",
        )));
    }
    // A route's only upstream needs no id to tell it apart, and only it may
    // forward the caller's credential: several use their own provider keys.
    let alone = raw_upstreams.len() == 1;
    let mut upstreams: Vec<Upstream> = Vec::with_capacity(raw_upstreams.len());
    for mut entry in raw_upstreams {
        let entry_id = entry.id.take();
        if entry_id.is_none() && !alone {
            return Err(in_route(id)(ConfigError::Invalid(
                "upstreams: every entry needs an id when the route has two or more",
            )));
        }
        if entry.forward_caller_credential && !alone {
            return Err(in_route(id)(ConfigError::Invalid(
                "upstreams: forward_caller_credential needs a route with one upstream: \
                 several upstreams use their own provider keys",
            )));
        }
        let in_entry = |error| ConfigError::RouteUpstream {
            route,
            id: entry_id.clone(),
            error: Box::new(error),
        };
        if entry.forward_caller_credential && id != RouteId::Claude {
            return Err(in_entry(ConfigError::Invalid(
                "forward_caller_credential belongs to routes.claude only",
            )));
        }
        if let Some(entry_id) = &entry_id {
            if !is_upstream_id(entry_id) {
                return Err(in_entry(ConfigError::Invalid(
                    "id must be 1-64 letters, digits, '.', '_', or '-'",
                )));
            }
            if upstreams.iter().any(|u| u.id.as_deref() == Some(entry_id)) {
                return Err(in_entry(ConfigError::Invalid("id is used twice")));
            }
        }
        let provider = match (provider, entry.provider.as_deref()) {
            (default, None) => default,
            (Provider::Anthropic, Some("anthropic")) => Provider::Anthropic,
            (Provider::Anthropic, Some("claude-platform-aws")) => Provider::ClaudePlatformAws,
            (Provider::Anthropic, Some("bedrock")) => Provider::Bedrock,
            (Provider::OpenAi, Some("openai")) => Provider::OpenAi,
            (Provider::OpenAi, Some(_)) => {
                return Err(in_entry(ConfigError::Invalid(
                    "provider must be \"openai\"",
                )));
            }
            (_, Some(_)) => {
                return Err(in_entry(ConfigError::Invalid(
                    "provider must be \"anthropic\", \"claude-platform-aws\", or \"bedrock\" \
                     (Vertex is planned)",
                )));
            }
        };
        let workspace_id = match (provider, entry.workspace_id.take()) {
            (Provider::ClaudePlatformAws, Some(id)) if is_workspace_id(&id) => Some(id.into()),
            (Provider::ClaudePlatformAws, Some(_)) => {
                return Err(in_entry(ConfigError::Invalid(
                    "workspace_id must be wrkspc_ followed by letters and digits",
                )));
            }
            (Provider::ClaudePlatformAws, None) => {
                return Err(in_entry(ConfigError::Invalid(
                    "claude-platform-aws needs workspace_id: every request names its workspace",
                )));
            }
            (_, Some(_)) => {
                return Err(in_entry(ConfigError::Invalid(
                    "workspace_id belongs to provider \"claude-platform-aws\" only",
                )));
            }
            (_, None) => None,
        };
        let weight = match entry.weight {
            None => 1,
            Some(w @ 1..=MAX_WEIGHT) => w,
            Some(_) => {
                return Err(in_entry(ConfigError::Invalid(
                    "weight must be between 1 and 1000",
                )));
            }
        };
        let can_forward = alone && id == RouteId::Claude;
        let mut upstream = validate_upstream(entry, provider, can_forward).map_err(in_entry)?;
        upstream.id = entry_id.map(Into::into);
        upstream.weight = weight;
        upstream.workspace_id = workspace_id;
        upstreams.push(upstream);
    }
    Ok(upstreams)
}

/// A Claude Platform on AWS workspace id (`wrkspc_01AbCd...`): sent as a header.
fn is_workspace_id(id: &str) -> bool {
    id.strip_prefix("wrkspc_").is_some_and(|rest| {
        (1..=64).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// Ids appear in telemetry attributes: short, printable, no separators.
fn is_upstream_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// One upstream's address, credential, and timers; `can_forward` when
/// forward_caller_credential is one of its credential choices.
fn validate_upstream(
    raw: RawUpstream,
    provider: Provider,
    can_forward: bool,
) -> Result<Upstream, ConfigError> {
    let provider_key = match (
        raw.provider_key_env,
        raw.provider_key_file,
        raw.forward_caller_credential,
    ) {
        (Some(name), None, false) => ProviderKey::Env(env_name(name, "provider_key_env")?),
        (None, Some(path), false) => {
            if path.as_os_str().is_empty() {
                return Err(ConfigError::Empty("provider_key_file"));
            }
            ProviderKey::File(path)
        }
        (None, None, true) => ProviderKey::ForwardCaller,
        (None, None, false) => {
            return Err(ConfigError::Invalid(if can_forward {
                "set provider_key_env, provider_key_file, or forward_caller_credential = true"
            } else {
                "set provider_key_env or provider_key_file"
            }));
        }
        _ => {
            return Err(ConfigError::Invalid(if can_forward {
                "set only one of provider_key_env, provider_key_file, and \
                 forward_caller_credential"
            } else {
                "set only one of provider_key_env and provider_key_file"
            }));
        }
    };
    let (scheme, authority, path_prefix) = parse_base_url("base_url", &raw.base_url)?;
    Ok(Upstream {
        id: None,
        weight: 1,
        provider,
        workspace_id: None,
        scheme,
        authority,
        path_prefix,
        provider_key,
        ca_file: match raw.ca_file {
            Some(path) if path.as_os_str().is_empty() => {
                return Err(ConfigError::Empty("ca_file"));
            }
            path => path,
        },
        connect_timeout: running_timer(
            raw.connect_timeout_secs
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
            "connect_timeout_secs",
        )?,
        response_header_timeout: running_timer(
            raw.response_header_timeout_secs
                .unwrap_or(DEFAULT_RESPONSE_HEADER_TIMEOUT_SECS),
            "response_header_timeout_secs",
        )?,
        http_version: match raw.http_version.as_deref() {
            None | Some("auto") => UpstreamHttpVersion::Auto,
            Some("1.1") => UpstreamHttpVersion::Http1,
            Some(_) => {
                return Err(ConfigError::Invalid(
                    "http_version must be \"auto\" or \"1.1\"",
                ));
            }
        },
        stream_idle_timeout: running_timer(
            raw.stream_idle_timeout_secs
                .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT_SECS),
            "stream_idle_timeout_secs",
        )?,
    })
}

/// `field` names the base URL in errors.
fn parse_base_url(
    field: &'static str,
    url: &str,
) -> Result<(Scheme, Authority, String), ConfigError> {
    let invalid = |reason| ConfigError::InvalidUrl {
        field,
        url: url.to_owned(),
        reason,
    };
    let (scheme, authority, path_prefix) = parse_url(field, url)?;
    if scheme == Scheme::HTTP && !is_loopback_host(authority.host()) {
        return Err(invalid("plain http is allowed only for loopback hosts"));
    }
    if path_prefix.ends_with("/v1") {
        return Err(invalid(
            "drop the trailing /v1: Wye forwards the client's /v1/... path after it",
        ));
    }
    Ok((scheme, authority, path_prefix))
}
