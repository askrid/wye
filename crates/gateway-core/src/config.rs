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

use crate::{alias::Aliases, api::Api, policy::ModelPattern, pricing::Pricing, route::Route};
use validate::{
    RawConfig, env_name, validate_aliases, validate_auth, validate_discovery, validate_pricing,
    validate_routes, validate_server, validate_telemetry, validate_upgrade_check,
};

/// Request bytes held process-wide while reading `model` for permissions.
const DEFAULT_MAX_HELD_REQUEST_BYTES: u64 = 64 * 1024 * 1024;
/// Anthropic's request size limit.
const DEFAULT_MAX_REQUEST_BODY_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 30;
const DEFAULT_CLIENT_WRITE_TIMEOUT_SECS: u64 = 60;
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
    /// Where each API's requests go; at least one route.
    pub routes: Routes,
    /// Gateway credentials callers must present. Without it, Wye listens on
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

/// One route per native API.
#[derive(Debug, Clone)]
pub struct Routes {
    /// Anthropic Messages: `[upstream]`, or `[routes.claude]`.
    pub claude: Option<Route>,
    /// OpenAI Responses: `[routes.openai]`.
    pub openai: Option<Route>,
}

impl Routes {
    /// The route serving `api`, if configured.
    pub fn get(&self, api: Api) -> Option<&Route> {
        match api {
            Api::Messages => self.claude.as_ref(),
            Api::Responses => self.openai.as_ref(),
        }
    }

    /// Every configured route with its API, Claude's first.
    pub fn iter(&self) -> impl Iterator<Item = (Api, &Route)> {
        [
            (Api::Messages, &self.claude),
            (Api::Responses, &self.openai),
        ]
        .into_iter()
        .filter_map(|(api, route)| route.as_ref().map(|r| (api, r)))
    }

    /// Every upstream of every route, in [`Self::iter`] order.
    pub fn upstreams(&self) -> impl Iterator<Item = &Upstream> {
        self.iter().flat_map(|(_, route)| route.upstreams())
    }
}

/// Most `upgrade_check.targets`.
pub const MAX_UPGRADE_TARGETS: usize = 16;

/// Upgrade checks on a sample of requests.
#[derive(Debug, Clone)]
pub struct UpgradeCheck {
    /// Models an alias or a client may move to; unique.
    pub targets: Box<[Box<str>]>,
    /// Share of Claude Code sessions checked, every request of each.
    pub sample_percent: u8,
}

/// Most catalog entries: Claude Code asks for `limit=1000`, and Wye serves
/// one page.
pub const MAX_DISCOVERY_MODELS: usize = 1000;
/// Longest `display_name` or `description`.
pub const MAX_DISCOVERY_TEXT_BYTES: usize = 1024;

/// The model catalog served at `GET /v1/models`, filtered per caller by
/// model permissions.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// In listing order; ids are unique.
    pub models: Vec<DiscoveredModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// The model name clients send; Claude Code keeps only ids containing
    /// `claude` or `anthropic`.
    pub id: String,
    pub display_name: Option<String>,
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

/// Signed gateway credentials (see [`crate::credential`]): Wye's own, from
/// identity providers, or both.
#[derive(Debug, Clone)]
pub struct Auth {
    /// Credentials issued with `wye token issue`; `None` when only identity
    /// providers are trusted.
    pub own: Option<OwnIssuer>,
    /// Identity providers whose tokens are accepted directly.
    pub issuers: Vec<Issuer>,
    /// Longest accepted `exp - iat`: without a revocation list, offboarding
    /// takes effect when the person's last credential expires.
    pub max_credential_ttl: Duration,
    /// Models any credential may use; `None` allows every model. A
    /// credential's `models` claim, or an issuer's grants, narrow this, never
    /// widen it.
    pub models: Option<Vec<ModelPattern>>,
    /// Largest `max_tokens` any credential may request on `/v1/messages`;
    /// a credential's `max_output_tokens` claim can only lower it.
    pub max_output_tokens: Option<u64>,
}

/// Wye's own credentials: Ed25519, `alg` pinned to `EdDSA`.
#[derive(Debug, Clone)]
pub struct OwnIssuer {
    /// Required `iss` claim.
    pub issuer: String,
    /// Required `aud` claim (one of them, if a list).
    pub audience: String,
    /// Verification keys, selected by the token's `kid`; several allow rotation.
    pub keys: Vec<PublicKey>,
}

#[derive(Debug, Clone)]
pub struct PublicKey {
    pub id: String,
    pub ed25519_public_key: [u8; 32],
}

/// An identity provider (`[[auth.issuers]]`) whose JWTs Wye verifies
/// directly, with keys pinned in config: no runtime key fetching.
#[derive(Debug, Clone)]
pub struct Issuer {
    /// Required `iss` claim.
    pub issuer: String,
    /// Required `aud` claim (one of them, if a list).
    pub audience: String,
    /// The one algorithm accepted from this issuer, never read from the token.
    pub algorithm: Algorithm,
    /// The issuer's JSON Web Key Set, relative to the config file; read at startup.
    pub jwks_file: PathBuf,
    /// The claim that names the credential's subject (default `sub`).
    pub subject_claim: String,
    /// Models granted by claim values. Empty: the `[auth] models` ceiling
    /// applies alone. Otherwise a token gets the union of its matching
    /// grants, and one matching none may use no model.
    pub grants: Vec<Grant>,
}

/// Signature algorithms accepted from identity providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// RSASSA-PKCS1-v1_5 with SHA-256, keys of 2048 to 8192 bits.
    Rs256,
    /// ECDSA on P-256 with SHA-256.
    Es256,
    /// Ed25519.
    EdDsa,
}

impl Algorithm {
    /// The JOSE `alg` name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Es256 => "ES256",
            Self::EdDsa => "EdDSA",
        }
    }
}

/// Models a token gets when its `claim` equals `value`, or is a list
/// containing it (for example `groups`).
#[derive(Debug, Clone)]
pub struct Grant {
    pub claim: String,
    pub value: String,
    pub models: Vec<ModelPattern>,
}

#[derive(Debug, Clone)]
pub struct Server {
    /// A loopback address unless `[auth]` is set.
    pub listen: SocketAddr,
    pub max_request_body_bytes: u64,
    /// Request bytes held across all requests while reading `model` and
    /// `max_tokens`; beyond it, requests are answered with an overload error.
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
    /// A client connection whose response writes make no progress for this
    /// long is closed, which cancels its upstream request: a client that
    /// stops reading would otherwise hold its stream and buffers indefinitely.
    pub client_write_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Upstream {
    /// Names the upstream in telemetry and `wye explain`; `None` for the
    /// single `[upstream]`.
    pub id: Option<Box<str>>,
    /// Share of affinity keys, relative to the route's other upstreams.
    pub weight: u32,
    pub provider: Provider,
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

/// Who serves an upstream, which decides how Wye authenticates to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Anthropic's API: the key goes in `x-api-key`.
    Anthropic,
    /// OpenAI's API: the key goes in `Authorization: Bearer`.
    OpenAi,
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
    /// A problem in one `[[routes.<route>.upstreams]]` entry.
    RouteUpstream {
        route: &'static str,
        id: String,
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
            Self::RouteUpstream { route, id, error } => {
                write!(f, "routes.{route}.upstreams {id:?}: {error}")
            }
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
        let routes = validate_routes(raw.upstream, raw.routes)?;
        if auth.is_some()
            && routes
                .upstreams()
                .any(|u| u.credential == UpstreamCredential::ForwardCaller)
        {
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
        let discovery = raw.discovery.map(validate_discovery).transpose()?;
        let aliases = validate_aliases(raw.aliases)?;
        let pricing = raw.pricing.map(validate_pricing).transpose()?;
        let upgrade_check = raw.upgrade_check.map(validate_upgrade_check).transpose()?;
        Ok(Self {
            server: validate_server(raw.server, auth.is_some())?,
            routes,
            auth,
            cache_diagnostics,
            telemetry,
            discovery,
            aliases,
            pricing,
            upgrade_check,
        })
    }
}

#[cfg(test)]
mod tests;
