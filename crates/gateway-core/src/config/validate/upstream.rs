//! `[[upstreams]]` and the routes they make: each entry serves the route its
//! provider speaks, Anthropic Messages or OpenAI Responses and Chat
//! Completions.

use serde::Deserialize;

use super::*;
use crate::route::{MAX_UPSTREAMS, MAX_WEIGHT};

/// One `[[upstreams]]` entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawUpstream {
    id: Option<String>,
    provider: Option<String>,
    weight: Option<u32>,
    #[serde(default)]
    standby: bool,
    workspace_id: Option<String>,
    base_url: Option<String>,
    provider_key: Option<RawSecret>,
    #[serde(default)]
    forward_caller_credential: bool,
    ca_file: Option<PathBuf>,
    connect_timeout: Option<String>,
    response_header_timeout: Option<String>,
    stream_idle_timeout: Option<String>,
    http_version: Option<String>,
}

/// The routes `[[upstreams]]` make: each entry in the route of its
/// provider, in config order; at least one entry.
pub(super) fn validate_routes(raw: Vec<RawUpstream>) -> Result<Routes, ConfigError> {
    if raw.is_empty() {
        return Err(ConfigError::Invalid(
            "add an [[upstreams]] entry: its provider (by default \"anthropic\") decides \
             which API it serves",
        ));
    }
    // An entry's id is its provider's name unless it names one, so a label
    // for each entry's errors is known before its provider is.
    let label = |entry: &RawUpstream| {
        entry
            .id
            .clone()
            .or_else(|| entry.provider.clone())
            .unwrap_or_else(|| Provider::DEFAULT.as_str().to_owned())
    };
    let providers = raw
        .iter()
        .map(|entry| match entry.provider.as_deref() {
            None => Ok(Provider::DEFAULT),
            Some(name) => Provider::parse(name).ok_or_else(|| ConfigError::Upstream {
                id: label(entry),
                error: Box::new(ConfigError::Invalid(
                    "provider must be \"anthropic\", \"claude-platform-aws\", \"bedrock\", \
                     or \"openai\" (Vertex is planned)",
                )),
            }),
        })
        .collect::<Result<Vec<_>, _>>()?;
    for route in RouteId::ALL {
        let entries = || {
            raw.iter()
                .zip(&providers)
                .filter(move |(_, p)| p.route() == route)
                .map(|(entry, _)| entry)
        };
        let problem = |problem| ConfigError::Route { route, problem };
        if entries().count() > MAX_UPSTREAMS {
            return Err(problem("takes at most 16 upstreams"));
        }
        if entries().next().is_some() && entries().all(|entry| entry.standby) {
            return Err(problem(
                "needs an upstream that is not standby: standbys take only failover",
            ));
        }
    }
    let mut upstreams: Vec<Upstream> = Vec::with_capacity(raw.len());
    for (entry, provider) in raw.into_iter().zip(providers.iter().copied()) {
        // The provider's name tells an upstream apart from the others unless
        // they share its provider.
        let shared = providers.iter().filter(|&&p| p == provider).count() > 1;
        if entry.id.is_none() && shared {
            return Err(ConfigError::Invalid(
                "upstreams: entries that share a provider each need an id",
            ));
        }
        let id = entry
            .id
            .clone()
            .unwrap_or_else(|| provider.as_str().to_owned());
        let in_entry = |error| ConfigError::Upstream {
            id: id.clone(),
            error: Box::new(error),
        };
        // Only a route's only upstream may forward the caller's credential:
        // several use their own provider keys.
        let alone = providers
            .iter()
            .filter(|p| p.route() == provider.route())
            .count()
            == 1;
        if entry.forward_caller_credential && !alone {
            return Err(in_entry(ConfigError::Invalid(
                "forward_caller_credential needs a route with one upstream: several \
                 upstreams use their own provider keys",
            )));
        }
        if !is_upstream_id(&id) {
            return Err(in_entry(ConfigError::Invalid(
                "id must be 1-64 letters, digits, '.', '_', or '-'",
            )));
        }
        // Ids name an upstream in usage records and price entries, across
        // routes.
        if upstreams.iter().any(|u| *u.id == *id) {
            return Err(in_entry(ConfigError::Invalid("id is used twice")));
        }
        upstreams.push(validate_upstream(entry, provider, &id, alone).map_err(in_entry)?);
    }
    let (claude, openai): (Vec<_>, Vec<_>) = upstreams
        .into_iter()
        .partition(|u| u.provider.route() == RouteId::Claude);
    let route = |upstreams: Vec<Upstream>| (!upstreams.is_empty()).then(|| Route::new(upstreams));
    Ok(Routes {
        claude: route(claude),
        openai: route(openai),
    })
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

/// The upstream `id`, from one `[[upstreams]]` entry of `provider`;
/// `can_forward` when forward_caller_credential is one of its choices of
/// provider key.
fn validate_upstream(
    raw: RawUpstream,
    provider: Provider,
    id: &str,
    can_forward: bool,
) -> Result<Upstream, ConfigError> {
    let workspace_id = match (provider, raw.workspace_id) {
        (Provider::ClaudePlatformAws, Some(ws)) if is_workspace_id(&ws) => Some(ws.into()),
        (Provider::ClaudePlatformAws, Some(_)) => {
            return Err(ConfigError::Invalid(
                "workspace_id must be wrkspc_ followed by letters and digits",
            ));
        }
        (Provider::ClaudePlatformAws, None) => {
            return Err(ConfigError::Invalid(
                "claude-platform-aws needs workspace_id: every request names its workspace",
            ));
        }
        (_, Some(_)) => {
            return Err(ConfigError::Invalid(
                "workspace_id belongs to provider \"claude-platform-aws\" only",
            ));
        }
        (_, None) => None,
    };
    let weight = match (raw.weight, raw.standby) {
        (None, _) => 1,
        (Some(_), true) => {
            return Err(ConfigError::Invalid(
                "weight does not apply to a standby: standbys take failover in config order",
            ));
        }
        (Some(w @ 1..=MAX_WEIGHT), false) => w,
        (Some(_), false) => {
            return Err(ConfigError::Invalid("weight must be between 1 and 1000"));
        }
    };
    let provider_key = match (raw.provider_key, raw.forward_caller_credential) {
        (Some(secret), false) => ProviderKey::Secret(validate_secret(secret, "provider_key")?),
        (None, true) => ProviderKey::ForwardCaller,
        (None, false) => {
            return Err(ConfigError::Invalid(if can_forward {
                "set provider_key = { env = \"NAME\" } or { file = \"path\" }, or \
                 forward_caller_credential = true"
            } else {
                "set provider_key = { env = \"NAME\" } or { file = \"path\" }"
            }));
        }
        (Some(_), true) => {
            return Err(ConfigError::Invalid(
                "set provider_key or forward_caller_credential, not both",
            ));
        }
    };
    let base_url = match (raw.base_url, provider.default_base_url()) {
        (Some(url), _) => url,
        (None, Some(url)) => url.to_owned(),
        (None, None) => {
            return Err(ConfigError::Invalid(match provider {
                Provider::Bedrock => {
                    "set base_url: a Bedrock URL names its region, such as \
                     https://bedrock-mantle.us-east-1.api.aws/anthropic"
                }
                _ => {
                    "set base_url: a Claude Platform on AWS URL names its region, such as \
                     https://aws-external-anthropic.us-west-2.api.aws"
                }
            }));
        }
    };
    let (scheme, authority, path_prefix) = parse_url("base_url", &base_url)?;
    if path_prefix.ends_with("/v1") {
        return Err(ConfigError::InvalidUrl {
            field: "base_url",
            url: base_url,
            reason: "drop the trailing /v1: Wye forwards the client's /v1/... path after it",
        });
    }
    Ok(Upstream {
        id: id.into(),
        weight,
        standby: raw.standby,
        provider,
        workspace_id,
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
        connect_timeout: running(
            raw.connect_timeout,
            DEFAULT_CONNECT_TIMEOUT,
            MAX_TIMER,
            "connect_timeout",
        )?,
        response_header_timeout: running(
            raw.response_header_timeout,
            DEFAULT_RESPONSE_HEADER_TIMEOUT,
            MAX_TIMER,
            "response_header_timeout",
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
        stream_idle_timeout: running(
            raw.stream_idle_timeout,
            DEFAULT_STREAM_IDLE_TIMEOUT,
            MAX_TIMER,
            "stream_idle_timeout",
        )?,
    })
}
