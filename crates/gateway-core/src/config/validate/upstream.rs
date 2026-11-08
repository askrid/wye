//! `[[upstreams]]` and the routes they make: each entry serves the route its
//! provider speaks, Anthropic Messages or OpenAI Responses and Chat
//! Completions.

use serde::Deserialize;

use super::*;
use crate::route::{MAX_UPSTREAMS, MAX_WEIGHT};

/// One `[[upstreams]]` entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawUpstream {
    pub(crate) id: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) weight: Option<u32>,
    #[serde(default)]
    pub(crate) standby: bool,
    pub(crate) workspace_id: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) provider_key: Option<RawSecret>,
    #[serde(default)]
    pub(crate) forward_caller_credential: bool,
    pub(crate) ca_file: Option<PathBuf>,
    pub(crate) connect_timeout: Option<String>,
    pub(crate) response_header_timeout: Option<String>,
    pub(crate) stream_idle_timeout: Option<String>,
    pub(crate) http_version: Option<String>,
}

/// The routes `[[upstreams]]` make: each entry in the route of its
/// provider, in config order; at least one entry.
pub(crate) fn validate_routes(raw: Vec<RawUpstream>) -> Result<Routes, ConfigError> {
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
        let problem = |problem| ConfigError::Route {
            route: route.label(),
            problem,
        };
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
    for (mut entry, provider) in raw.into_iter().zip(providers.iter().copied()) {
        // The provider's name tells an upstream apart from the others unless
        // they share its provider.
        let shared = providers.iter().filter(|&&p| p == provider).count() > 1;
        if entry.id.is_none() && shared {
            return Err(ConfigError::Invalid(
                "upstreams: entries that share a provider each need an id",
            ));
        }
        let entry_id = entry
            .id
            .take()
            .unwrap_or_else(|| provider.as_str().to_owned());
        let in_entry = |error| ConfigError::Upstream {
            id: entry_id.clone(),
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
        if !is_upstream_id(&entry_id) {
            return Err(in_entry(ConfigError::Invalid(
                "id must be 1-64 letters, digits, '.', '_', or '-'",
            )));
        }
        // Ids name an upstream in usage records and price entries, across
        // routes.
        if upstreams.iter().any(|u| *u.id == *entry_id) {
            return Err(in_entry(ConfigError::Invalid("id is used twice")));
        }
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
        let weight = match (entry.weight, entry.standby) {
            (None, _) => 1,
            (Some(_), true) => {
                return Err(in_entry(ConfigError::Invalid(
                    "weight does not apply to a standby: standbys take failover in config \
                     order",
                )));
            }
            (Some(w @ 1..=MAX_WEIGHT), false) => w,
            (Some(_), false) => {
                return Err(in_entry(ConfigError::Invalid(
                    "weight must be between 1 and 1000",
                )));
            }
        };
        let standby = entry.standby;
        let mut upstream = validate_upstream(entry, provider, alone).map_err(in_entry)?;
        upstream.id = entry_id.into();
        upstream.weight = weight;
        upstream.standby = standby;
        upstream.workspace_id = workspace_id;
        upstreams.push(upstream);
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

/// One upstream's address, credential, and timers; `can_forward` when
/// forward_caller_credential is one of its credential choices.
fn validate_upstream(
    raw: RawUpstream,
    provider: Provider,
    can_forward: bool,
) -> Result<Upstream, ConfigError> {
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
    let (scheme, authority, path_prefix) = parse_base_url("base_url", &base_url)?;
    Ok(Upstream {
        id: "".into(),
        weight: 1,
        standby: false,
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
            raw.connect_timeout,
            DEFAULT_CONNECT_TIMEOUT,
            "connect_timeout",
        )?,
        response_header_timeout: running_timer(
            raw.response_header_timeout,
            DEFAULT_RESPONSE_HEADER_TIMEOUT,
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
        stream_idle_timeout: running_timer(
            raw.stream_idle_timeout,
            DEFAULT_STREAM_IDLE_TIMEOUT,
            "stream_idle_timeout",
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
    if path_prefix.ends_with("/v1") {
        return Err(invalid(
            "drop the trailing /v1: Wye forwards the client's /v1/... path after it",
        ));
    }
    Ok((scheme, authority, path_prefix))
}
