//! Claude Code's agent headers, captured on the request path and validated in
//! the writer task (see [`gateway_core::agent`]).

use gateway_core::agent::{self, Compaction, RequestClass};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde::ser::{Serialize, SerializeSeq, Serializer};

const AGENT_ID: HeaderName = HeaderName::from_static("x-claude-code-agent-id");
const PARENT_AGENT_ID: HeaderName = HeaderName::from_static("x-claude-code-parent-agent-id");
const REQUEST_CLASS: HeaderName = HeaderName::from_static("x-claude-code-request-class");
const AGENT_TYPE: HeaderName = HeaderName::from_static("x-claude-code-agent-type");
const COMPACTION: HeaderName = HeaderName::from_static("x-claude-code-compaction");
const CONTEXT_COMPACTED: HeaderName = HeaderName::from_static("x-claude-code-context-compacted");
const PROMPT_ID: HeaderName = HeaderName::from_static("x-claude-code-prompt-id");
const PREV_TOOL_DURATIONS: HeaderName =
    HeaderName::from_static("x-claude-code-prev-tool-durations");

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
    prev_tool_durations: Option<HeaderValue>,
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
    pub(super) tool_durations: Option<ToolDurations<'a>>,
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
            prev_tool_durations: headers.get(PREV_TOOL_DURATIONS).cloned(),
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
            tool_durations: self
                .prev_tool_durations
                .as_ref()
                .map(|v| ToolDurations(v.as_bytes())),
        }
    }
}

/// The raw `x-claude-code-prev-tool-durations` value, parsed where it is
/// written (see [`agent::tool_durations`]).
#[derive(Clone, Copy)]
pub(super) struct ToolDurations<'a>(pub(super) &'a [u8]);

impl<'a> ToolDurations<'a> {
    pub(super) fn iter(self) -> impl Iterator<Item = (std::borrow::Cow<'a, str>, u64)> {
        agent::tool_durations(self.0)
    }
}

/// A list of `{"name": ..., "ms": ...}` objects.
impl Serialize for ToolDurations<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Entry<'n> {
            name: &'n str,
            ms: u64,
        }
        let mut seq = serializer.serialize_seq(None)?;
        for (name, ms) in self.iter() {
            seq.serialize_element(&Entry { name: &name, ms })?;
        }
        seq.end()
    }
}
