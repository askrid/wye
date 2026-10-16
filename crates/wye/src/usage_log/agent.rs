//! Claude Code's agent headers, captured on the request path and validated in
//! the writer task (see [`gateway_core::agent`]).

use gateway_core::agent::{self, Compaction, RequestClass};
use http::{HeaderMap, HeaderName, HeaderValue};

const AGENT_ID: HeaderName = HeaderName::from_static("x-claude-code-agent-id");
const PARENT_AGENT_ID: HeaderName = HeaderName::from_static("x-claude-code-parent-agent-id");
const REQUEST_CLASS: HeaderName = HeaderName::from_static("x-claude-code-request-class");
const AGENT_TYPE: HeaderName = HeaderName::from_static("x-claude-code-agent-type");
const COMPACTION: HeaderName = HeaderName::from_static("x-claude-code-compaction");
const CONTEXT_COMPACTED: HeaderName = HeaderName::from_static("x-claude-code-context-compacted");
const PROMPT_ID: HeaderName = HeaderName::from_static("x-claude-code-prompt-id");

/// The agent headers of one request. Closed vocabularies are parsed here
/// (a few byte comparisons); open values are kept as received, sharing the
/// request's header bytes, and validated only when the record is written.
#[derive(Default)]
pub(crate) struct AgentHeaders {
    request_class: Option<RequestClass>,
    compaction: Option<Compaction>,
    context_compacted: Option<Compaction>,
    agent_id: Option<HeaderValue>,
    parent_agent_id: Option<HeaderValue>,
    agent_type: Option<HeaderValue>,
    prompt_id: Option<HeaderValue>,
}

/// The validated values a usage record carries.
pub(super) struct AgentFields<'a> {
    pub(super) request_class: Option<&'static str>,
    pub(super) agent_type: Option<&'a str>,
    pub(super) agent_id: Option<&'a str>,
    pub(super) parent_agent_id: Option<&'a str>,
    pub(super) prompt_id: Option<&'a str>,
    pub(super) compaction: Option<Compaction>,
    pub(super) context_compacted: Option<Compaction>,
}

fn token(value: Option<&HeaderValue>) -> Option<&str> {
    value.and_then(|v| agent::token(v.as_bytes()))
}

impl AgentHeaders {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        let bytes = |name| headers.get(name).map(HeaderValue::as_bytes);
        Self {
            request_class: RequestClass::parse(bytes(REQUEST_CLASS)),
            compaction: Compaction::parse(bytes(COMPACTION)),
            context_compacted: Compaction::parse(bytes(CONTEXT_COMPACTED)),
            agent_id: headers.get(AGENT_ID).cloned(),
            parent_agent_id: headers.get(PARENT_AGENT_ID).cloned(),
            agent_type: headers.get(AGENT_TYPE).cloned(),
            prompt_id: headers.get(PROMPT_ID).cloned(),
        }
    }

    pub(super) fn fields(&self) -> AgentFields<'_> {
        AgentFields {
            request_class: self.request_class.map(RequestClass::as_str),
            agent_type: token(self.agent_type.as_ref()),
            agent_id: token(self.agent_id.as_ref()),
            parent_agent_id: token(self.parent_agent_id.as_ref()),
            prompt_id: token(self.prompt_id.as_ref()),
            compaction: self.compaction,
            context_compacted: self.context_compacted,
        }
    }
}
