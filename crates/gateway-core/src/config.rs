//! `wye.toml` parsing and validation.
//!
//! Raw TOML is parsed into private `Raw*` types and validated once into public
//! types whose invariants later code relies on. Secrets and files are
//! referenced by environment variable name or path ([`Secret`]) and never read
//! here.

use std::{fmt, net::SocketAddr, path::PathBuf, time::Duration};

use http::{
    Uri,
    uri::{Authority, Scheme},
};

mod auth;
mod secret;
mod upstream;
mod validate;

pub use auth::{Algorithm, Auth, Grant, IdentityProvider, OWN_ISSUER, OwnIssuer, PublicKey};
pub use secret::Secret;
pub use upstream::{Provider, ProviderKey, Upstream, UpstreamHttpVersion};

use crate::{
    alias::Aliases,
    api::Api,
    policy::ModelPattern,
    pricing::Pricing,
    route::{Route, RouteId},
};

/// The default `server.listen`, as a literal for `concat!`: also the
/// gateway the CLI's clients use without `--url` or `$WYE_URL`.
#[macro_export]
macro_rules! default_listen {
    () => {
        "127.0.0.1:8080"
    };
}
/// The default `server.listen`: this machine only, port 8080.
pub const DEFAULT_LISTEN: &str = default_listen!();
/// Anthropic's request size limit.
const DEFAULT_MAX_REQUEST_BODY_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// Generous for one process: an idle Claude Code stream costs Wye tens of KB.
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 10_000;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Non-streaming generations send no headers until they complete.
const DEFAULT_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(600);
/// Must exceed the provider's SSE `ping` interval during long thinking pauses.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Local use needs no drain; behind a load balancer, set it above the
/// health-check interval.
const DEFAULT_DRAIN_DELAY: Duration = Duration::ZERO;
const DEFAULT_MAX_GATEWAY_TOKEN_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// The longest `max_gateway_token_ttl`, 30 days: tokens cannot be revoked
/// one by one, so a stolen one is good until it expires.
pub(crate) const MAX_GATEWAY_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// The most open client connections, about a million: Linux's default
/// ceiling on one process's open files (`fs.nr_open`).
pub const MAX_CONNECTIONS: usize = 1 << 20;
/// Request bytes kept so failover can resend them, across all requests of a
/// process; beyond it, a request goes without failover, never refused.
pub const MAX_TOTAL_KEPT_BYTES: u64 = 256 * 1024 * 1024;
/// A client that accepts no response bytes this long is disconnected, which
/// cancels its upstream request: a client that stops reading would
/// otherwise hold its stream and buffers indefinitely.
pub const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Default `sample_percent` of cache diagnostics and upgrade checks: every
/// session.
pub const DEFAULT_SAMPLE_PERCENT: u8 = 100;

/// Validated gateway configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The listener and its limits, from `[server]`.
    pub server: Server,
    /// Where each API's requests go, from `[[upstreams]]`; at least one
    /// route.
    pub routes: Routes,
    /// Gateway tokens callers must present. Without it, Wye listens on
    /// loopback only.
    pub auth: Option<Auth>,
    /// Prompt hashes in the usage log, to explain prompt-cache misses.
    pub cache_diagnostics: Option<CacheDiagnostics>,
    /// OTLP export of usage records.
    pub telemetry: Option<Telemetry>,
    /// Models listed at `GET /v1/models`; without it the endpoint is absent.
    pub discovery: Option<Discovery>,
    /// `[[aliases]]`: names clients send, resolved to the model that runs.
    pub aliases: Option<Aliases>,
    /// `[pricing]`: the cost of each request in its usage record.
    pub pricing: Option<Pricing>,
    /// `[upgrade_check]`: which requests each target model would reject.
    pub upgrade_check: Option<UpgradeCheck>,
}

/// One route per native API: the config's [`Route`]s, or what a runtime
/// builds from each. Each `[[upstreams]]` entry is in the route its
/// provider serves ([`Provider::route`]).
#[derive(Debug, Clone)]
pub struct Routes<T = Route> {
    /// Anthropic Messages: the upstreams of providers `anthropic`,
    /// `claude-platform-aws`, and `bedrock`.
    pub claude: Option<T>,
    /// OpenAI Responses and Chat Completions: the upstreams of provider
    /// `openai`.
    pub openai: Option<T>,
}

impl<T> Routes<T> {
    /// The route serving `api`, if configured.
    pub fn get(&self, api: Api) -> Option<&T> {
        self.route(api.route())
    }

    /// The route `id`, if configured.
    pub fn route(&self, id: RouteId) -> Option<&T> {
        match id {
            RouteId::Claude => self.claude.as_ref(),
            RouteId::OpenAi => self.openai.as_ref(),
        }
    }

    /// Every configured route with its id, Claude's first.
    pub fn iter(&self) -> impl Iterator<Item = (RouteId, &T)> {
        RouteId::ALL
            .into_iter()
            .filter_map(|id| self.route(id).map(|route| (id, route)))
    }

    /// Each route through `build`, Claude's first; the first error ends it.
    pub fn try_map<U, E>(&self, mut build: impl FnMut(&T) -> Result<U, E>) -> Result<Routes<U>, E> {
        Ok(Routes {
            claude: self.claude.as_ref().map(&mut build).transpose()?,
            openai: self.openai.as_ref().map(&mut build).transpose()?,
        })
    }
}

impl Routes {
    /// Every upstream of every route, in [`Self::iter`] order.
    pub fn upstreams(&self) -> impl Iterator<Item = &Upstream> {
        self.iter().flat_map(|(_, route)| route.upstreams())
    }
}

/// Most `upgrade_check.targets`.
pub(crate) const MAX_UPGRADE_TARGETS: usize = 16;

/// Upgrade checks on a sample of requests.
#[derive(Debug, Clone)]
pub struct UpgradeCheck {
    /// Models an alias or a client may move to; unique.
    pub targets: Box<[Box<str>]>,
    /// Share of sessions checked (by session id, else token subject), every
    /// request of each.
    pub sample_percent: u8,
}

/// Most catalog entries: Claude Code asks for `limit=1000`, and Wye serves
/// one page.
pub(crate) const MAX_DISCOVERY_MODELS: usize = 1000;
/// Longest `display_name` or `description`.
pub(crate) const MAX_DISCOVERY_TEXT_BYTES: usize = 1024;

/// The model catalog served at `GET /v1/models`, filtered per caller by
/// model permissions.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// In listing order; ids are unique.
    pub models: Vec<DiscoveredModel>,
}

/// One `[discovery] models` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// The model name clients send; Claude Code keeps only ids containing
    /// `claude` or `anthropic`.
    pub id: String,
    /// The name clients show, at most 1,024 bytes.
    pub display_name: Option<String>,
    /// A line clients show beside it, at most 1,024 bytes.
    pub description: Option<String>,
}

/// OTLP export: each usage record is also sent as an OTLP log record and a
/// span, and aggregated into OTLP metrics.
#[derive(Debug, Clone)]
pub struct Telemetry {
    /// Logs endpoint: the configured base URL with `/v1/logs` appended.
    pub otlp_logs_uri: Uri,
    /// Metrics endpoint: the base URL with `/v1/metrics` appended.
    pub otlp_metrics_uri: Uri,
    /// Traces endpoint: the base URL with `/v1/traces` appended.
    pub otlp_traces_uri: Uri,
    /// Extra request headers, in the OpenTelemetry format
    /// `key1=value1,key2=value2` (for example a backend API key).
    pub otlp_headers: Option<Secret>,
    /// `service.name` resource attribute.
    pub service_name: String,
}

/// Cache-miss diagnostics (see [`crate::prompt`]).
#[derive(Debug, Clone)]
pub struct CacheDiagnostics {
    /// The secret that keys the exported hashes (at least
    /// [`crate::prompt::HashKey::MIN_LEN`] bytes). Required: an unkeyed hash
    /// could be matched against guessed prompts.
    pub hash_key: Secret,
    /// Percentage of Claude Code sessions diagnosed, 1-100 (see
    /// [`crate::route::sampled`]).
    pub sample_percent: u8,
}

/// The listener and its limits (`[server]`).
#[derive(Debug, Clone)]
pub struct Server {
    /// A loopback address unless `[auth]` is set.
    pub listen: SocketAddr,
    /// Largest request body; a larger one is refused with `413`.
    pub max_request_body_bytes: u64,
    /// Requests in flight (until their response body ends) before new
    /// ones are answered with an overload error instead of queueing.
    pub max_concurrent_requests: usize,
    /// After a shutdown signal, `/readyz` fails but connections are still
    /// accepted for this long, so load balancers stop routing here before
    /// new connections are refused. Zero for local use.
    pub drain_delay: Duration,
    /// After the drain delay, active requests get this long to finish.
    pub shutdown_grace: Duration,
}

impl Server {
    /// Request bytes held across all requests while reading `model`; beyond
    /// it, requests are answered with an overload error. Twice
    /// `max_request_body_bytes`: a body that names its model last is held
    /// whole, and with a smaller budget it would be refused, and retried,
    /// forever.
    pub fn max_total_held_bytes(&self) -> u64 {
        self.max_request_body_bytes.saturating_mul(2)
    }

    /// Open client connections, idle ones included; at the limit, Wye
    /// accepts no more until one closes, and new ones wait in the kernel's
    /// listen queue. Twice `max_concurrent_requests`, for the idle
    /// connections clients keep, at most [`MAX_CONNECTIONS`].
    pub fn max_connections(&self) -> usize {
        self.max_concurrent_requests
            .saturating_mul(2)
            .min(MAX_CONNECTIONS)
    }
}

/// Why a config is refused. A `&'static str` names the config key, as in
/// `server.drain_delay`; [`fmt::Display`] writes the message the CLI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Not TOML, or not the config's shape: the TOML parser's message.
    Parse(String),
    /// `server.listen` is not a socket address.
    InvalidListen(String),
    /// `server.listen` is not loopback, and there is no `[auth]`.
    NonLoopbackListen(SocketAddr),
    /// A problem in `[auth]`.
    Auth(String),
    /// A URL that is invalid, or not allowed where it is.
    InvalidUrl {
        /// The config key.
        field: &'static str,
        /// The URL as written.
        url: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A secret's `env` that is not an environment variable name.
    InvalidEnvName {
        /// The config key.
        field: &'static str,
        /// The name as written.
        name: String,
    },
    /// A secret naming no source, or both.
    Secret(&'static str),
    /// An empty string or path.
    Empty(&'static str),
    /// Zero where the value must be positive.
    Zero(&'static str),
    /// Not a duration as [`crate::duration::parse`] reads one.
    Duration(&'static str),
    /// A duration longer than `max`.
    TooLong {
        /// The config key.
        field: &'static str,
        /// The longest allowed.
        max: Duration,
    },
    /// A sampling share outside 1-100.
    Percent(&'static str),
    /// Any other problem, as a whole message.
    Invalid(&'static str),
    /// A problem with the upstreams that serve one route.
    Route {
        /// The route.
        route: RouteId,
        /// What is wrong, as a predicate: "takes at most 16 upstreams".
        problem: &'static str,
    },
    /// A problem in one `[[upstreams]]` entry, named by its id, written or
    /// by default.
    Upstream {
        /// The entry's id.
        id: String,
        /// The problem.
        error: Box<ConfigError>,
    },
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
            Self::InvalidUrl { field, url, reason } => write!(f, "{field} {url:?}: {reason}"),
            Self::InvalidEnvName { field, name } => {
                write!(
                    f,
                    "{field}: {name:?} is not a valid environment variable name"
                )
            }
            Self::Secret(field) => write!(
                f,
                "{field} must name one source: {{ env = \"NAME\" }} or {{ file = \"path\" }}"
            ),
            Self::Empty(field) => write!(f, "{field} must not be empty"),
            Self::Zero(field) => write!(f, "{field} must be greater than zero"),
            Self::Duration(field) => write!(
                f,
                concat!("{} must be a duration such as ", crate::duration_syntax!()),
                field
            ),
            Self::TooLong { field, max } => {
                write!(f, "{field} must be at most {}", crate::duration::show(*max))
            }
            Self::Percent(field) => write!(f, "{field} must be between 1 and 100"),
            Self::Invalid(message) => f.write_str(message),
            Self::Route { route, problem } => {
                write!(f, "upstreams: the {} route {problem}", route.label())
            }
            Self::Upstream { id, error } => write!(f, "upstreams {id:?}: {error}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Whether a URL's host is this machine (`localhost`, `127.0.0.1`, `[::1]`):
/// the only hosts plain http may reach without crossing a network.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether `uri` is plain http to another machine, where whatever a request
/// carries (a token, a key, a login) crosses the network in the clear.
pub fn is_cleartext_remote(uri: &Uri) -> bool {
    uri.scheme_str() == Some("http") && !uri.host().is_some_and(is_loopback_host)
}

impl Config {
    /// Parses and validates a TOML document.
    pub fn from_toml_str(source: &str) -> Result<Self, ConfigError> {
        toml::from_str(source)
            .map_err(|e| ConfigError::Parse(e.to_string()))
            .and_then(validate::config)
    }
}

#[cfg(test)]
mod tests;
