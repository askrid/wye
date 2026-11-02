//! The upstreams a route sends requests to: their providers, addresses,
//! and credentials.

use super::*;

#[derive(Debug, Clone)]
pub struct Upstream {
    /// Names the upstream in telemetry and `wye explain`; `None` for the
    /// single `[upstream]`.
    pub id: Option<Box<str>>,
    /// Share of affinity keys, relative to the route's other upstreams.
    pub weight: u32,
    pub provider: Provider,
    /// The `anthropic-workspace-id` sent with every request; set exactly
    /// for [`Provider::ClaudePlatformAws`].
    pub workspace_id: Option<Box<str>>,
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

/// Where the key comes from: `$NAME`, a file, or the caller.
impl fmt::Display for UpstreamCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKeyEnv(name) => write!(f, "${name}"),
            Self::ApiKeyFile(path) => write!(f, "{}", path.display()),
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
