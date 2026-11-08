//! The upstreams a route sends requests to: their providers, base URLs,
//! provider keys, and shares of the route's sessions.

use super::*;

#[derive(Debug, Clone)]
pub struct Upstream {
    /// Names the upstream in usage records, telemetry, prices, and `wye
    /// explain`: as written, else its provider's name ([`Provider::as_str`]).
    pub id: Box<str>,
    /// Share of affinity keys, relative to the route's other upstreams that
    /// are not standby; 1, and unused, for a standby.
    pub weight: u32,
    /// Takes requests only when failover has moved past every upstream of
    /// the route that is not standby, in config order.
    pub standby: bool,
    pub provider: Provider,
    /// The `anthropic-workspace-id` sent with every request; set exactly
    /// for [`Provider::ClaudePlatformAws`].
    pub workspace_id: Option<Box<str>>,
    pub scheme: Scheme,
    pub authority: Authority,
    /// Base path without a trailing slash; empty for a bare origin.
    pub path_prefix: String,
    pub provider_key: ProviderKey,
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
    /// Claude Platform on AWS: Anthropic's API, operated by Anthropic in
    /// AWS; the key goes in `Authorization: Bearer`, and every request names
    /// the workspace (`anthropic-workspace-id`).
    ClaudePlatformAws,
    /// Amazon Bedrock's Anthropic Messages endpoint: the Bedrock API key goes
    /// in `x-api-key`, and model ids carry the `anthropic.` prefix.
    Bedrock,
    /// OpenAI's API: the key goes in `Authorization: Bearer`.
    OpenAi,
}

impl Provider {
    /// The provider an upstream has unless it names one.
    pub const DEFAULT: Self = Self::Anthropic;

    /// The provider `name` is in config.
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::Anthropic,
            Self::ClaudePlatformAws,
            Self::Bedrock,
            Self::OpenAi,
        ]
        .into_iter()
        .find(|p| p.as_str() == name)
    }

    /// The route whose API the provider speaks.
    pub fn route(self) -> RouteId {
        match self {
            Self::Anthropic | Self::ClaudePlatformAws | Self::Bedrock => RouteId::Claude,
            Self::OpenAi => RouteId::OpenAi,
        }
    }

    /// The provider that a request refused before an upstream was chosen is
    /// recorded under: the first that speaks `route`'s API.
    pub fn of_route(route: RouteId) -> Self {
        match route {
            RouteId::Claude => Self::Anthropic,
            RouteId::OpenAi => Self::OpenAi,
        }
    }

    /// Its name in config, and the id of an upstream written without one.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::ClaudePlatformAws => "claude-platform-aws",
            Self::Bedrock => "bedrock",
            Self::OpenAi => "openai",
        }
    }

    /// The base URL of an upstream written without one; `None` where the
    /// URL names a region, so config must.
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::OpenAi => Some("https://api.openai.com"),
            Self::Bedrock | Self::ClaudePlatformAws => None,
        }
    }

    /// The OpenTelemetry `gen_ai.provider.name`: Claude Platform on AWS is
    /// Anthropic's own API, so it reads `anthropic`.
    pub fn otel_name(self) -> &'static str {
        match self {
            Self::Anthropic | Self::ClaudePlatformAws => "anthropic",
            Self::Bedrock => "aws.bedrock",
            Self::OpenAi => "openai",
        }
    }

    /// Whether the provider key goes as `Authorization: Bearer` rather than
    /// `x-api-key`.
    pub fn bearer_key(self) -> bool {
        matches!(self, Self::OpenAi | Self::ClaudePlatformAws)
    }

    /// How the key is sent, for diagnostics.
    pub fn key_header(self) -> &'static str {
        if self.bearer_key() {
            "Authorization: Bearer"
        } else {
            "x-api-key"
        }
    }

    /// The prefix this provider's model ids carry: `model` is rewritten
    /// with it for requests that go there (the allowlisted model rewrite).
    pub fn model_prefix(self) -> Option<&'static str> {
        match self {
            Self::Bedrock => Some("anthropic."),
            Self::Anthropic | Self::ClaudePlatformAws | Self::OpenAi => None,
        }
    }

    /// The prefix `model` needs at this provider: none when the provider has
    /// none, or when the client already sent the prefixed id.
    pub fn prefix_for(self, model: &str) -> Option<&'static str> {
        self.model_prefix()
            .filter(|prefix| !model.starts_with(prefix))
    }
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

/// The provider key Wye presents to an upstream: a configured secret, or,
/// with caller-credential passthrough, the caller's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderKey {
    /// Read at startup from the environment or a file.
    Secret(Secret),
    /// Forward the caller's own credential (loopback only).
    ForwardCaller,
}

/// Where the key comes from: `$NAME`, a file, or the caller.
impl fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Secret(secret) => secret.fmt(f),
            Self::ForwardCaller => f.write_str("the caller's own"),
        }
    }
}

impl Upstream {
    /// The base URL, without a trailing slash, as reports show it.
    pub fn base_url(&self) -> String {
        format!("{}://{}{}", self.scheme, self.authority, self.path_prefix)
    }
}
