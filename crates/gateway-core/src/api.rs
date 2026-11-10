//! The native APIs Wye forwards, and the few things that differ between
//! them. Requests are never translated from one to the other.

use crate::route::RouteId;

/// A native API, served by its own route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    /// Anthropic Messages (the Claude route): Claude Code and the Anthropic
    /// SDKs.
    Messages,
    /// OpenAI Responses (the OpenAI route): Codex and the OpenAI SDKs.
    Responses,
    /// OpenAI Chat Completions (the OpenAI route): OpenAI SDK clients and
    /// OpenAI-compatible agents.
    ChatCompletions,
}

/// A proxied endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    /// The native API it belongs to.
    pub api: Api,
    /// The request path, as a static string for records.
    pub path: &'static str,
    /// Whether the endpoint generates output, so an output-token ceiling
    /// applies (`count_tokens` does not).
    pub generates: bool,
}

/// The endpoint for `POST path`, if Wye proxies it.
pub fn endpoint(path: &str) -> Option<Endpoint> {
    let (api, path, generates) = match path {
        "/v1/messages" => (Api::Messages, "/v1/messages", true),
        "/v1/messages/count_tokens" => (Api::Messages, "/v1/messages/count_tokens", false),
        "/v1/responses" => (Api::Responses, "/v1/responses", true),
        "/v1/chat/completions" => (Api::ChatCompletions, "/v1/chat/completions", true),
        _ => return None,
    };
    Some(Endpoint {
        api,
        path,
        generates,
    })
}

impl Api {
    /// The top-level request field that bounds generated tokens, as named
    /// in error messages and diagnostics.
    pub fn output_token_field(self) -> &'static str {
        match self {
            Self::Messages => "max_tokens",
            Self::Responses => "max_output_tokens",
            Self::ChatCompletions => "max_completion_tokens",
        }
    }

    /// Every top-level field that bounds generated tokens: Chat Completions
    /// keeps the older `max_tokens` beside `max_completion_tokens`. A body
    /// naming two of them is refused, as one naming the same twice.
    pub(crate) fn output_token_fields(self) -> &'static [&'static str] {
        match self {
            Self::Messages => &["max_tokens"],
            Self::Responses => &["max_output_tokens"],
            Self::ChatCompletions => &["max_completion_tokens", "max_tokens"],
        }
    }

    /// Whether an output-token ceiling refuses a request without the token
    /// field. Anthropic requires `max_tokens`, so the provider rejects such a
    /// request itself; on OpenAI's APIs the field is optional and its absence
    /// means the model's maximum, which no ceiling can allow.
    pub(crate) fn output_token_field_required(self) -> bool {
        self != Self::Messages
    }

    /// The header with the client's session id: Claude Code's or Codex's.
    pub fn session_header(self) -> Option<&'static str> {
        match self {
            Self::Messages => Some("x-claude-code-session-id"),
            Self::Responses => Some("session-id"),
            Self::ChatCompletions => None,
        }
    }

    /// The route that serves this API.
    pub fn route(self) -> RouteId {
        match self {
            Self::Messages => RouteId::Claude,
            Self::Responses | Self::ChatCompletions => RouteId::OpenAi,
        }
    }
}
