//! The native APIs Wye forwards, and the few things that differ between
//! them. Requests are never translated from one to the other.

/// A native API, served by its own route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    /// Anthropic Messages (`[upstream]` or `[routes.claude]`): Claude Code
    /// and the Anthropic SDKs.
    Messages,
    /// OpenAI Responses (`[routes.openai]`): Codex and the OpenAI SDKs.
    Responses,
}

/// A proxied endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
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
        _ => return None,
    };
    Some(Endpoint {
        api,
        path,
        generates,
    })
}

impl Api {
    /// The top-level request field that bounds generated tokens.
    pub fn token_field(self) -> &'static str {
        match self {
            Self::Messages => "max_tokens",
            Self::Responses => "max_output_tokens",
        }
    }

    /// Whether a request without the token field may pass a token ceiling.
    /// Anthropic requires `max_tokens`, so the provider rejects such a
    /// request itself; on Responses the field is optional and its absence
    /// means the model's maximum, so a ceiling refuses it.
    pub fn token_field_required(self) -> bool {
        self == Self::Responses
    }

    /// The header with the client's session id: Claude Code's or Codex's.
    pub fn session_header(self) -> &'static str {
        match self {
            Self::Messages => "x-claude-code-session-id",
            Self::Responses => "session-id",
        }
    }

    /// The OpenTelemetry `gen_ai.system` value.
    pub fn system(self) -> &'static str {
        match self {
            Self::Messages => "anthropic",
            Self::Responses => "openai",
        }
    }

    /// The route that serves the API, as named in config.
    pub fn route_name(self) -> &'static str {
        match self {
            Self::Messages => "claude",
            Self::Responses => "openai",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxied_endpoints_and_their_apis() {
        let messages = endpoint("/v1/messages").unwrap();
        assert_eq!((messages.api, messages.generates), (Api::Messages, true));
        let count = endpoint("/v1/messages/count_tokens").unwrap();
        assert_eq!((count.api, count.generates), (Api::Messages, false));
        let responses = endpoint("/v1/responses").unwrap();
        assert_eq!((responses.api, responses.generates), (Api::Responses, true));
        for other in [
            "/v1/chat/completions",
            "/v1/models",
            "/v1/responses/abc",
            "/",
        ] {
            assert_eq!(endpoint(other), None, "{other}");
        }
    }
}
