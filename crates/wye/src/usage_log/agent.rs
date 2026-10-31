//! Agent headers (Claude Code's, or Codex's mapped onto the same fields),
//! captured on the request path and validated in the writer task (see
//! [`gateway_core::agent`]).

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
/// Codex: the thread (one agent's conversation), its parent for spawned
/// threads, the subagent kind, and the client name (`codex_cli_rs`, ...).
const CODEX_THREAD_ID: HeaderName = HeaderName::from_static("thread-id");
const CODEX_PARENT_THREAD_ID: HeaderName = HeaderName::from_static("x-codex-parent-thread-id");
const CODEX_SUBAGENT: HeaderName = HeaderName::from_static("x-openai-subagent");
const ORIGINATOR: HeaderName = HeaderName::from_static("originator");

/// The agent headers of one request. Closed vocabularies are parsed here
/// (a few byte comparisons); open values are kept as received, sharing the
/// request's header bytes, and validated only when the record is written.
#[derive(Default)]
pub(crate) struct AgentHeaders {
    pub(super) request_class: Option<RequestClass>,
    pub(super) compaction: Option<Compaction>,
    pub(super) context_compacted: Option<Compaction>,
    pub(super) agent_id: Option<HeaderValue>,
    pub(super) parent_agent_id: Option<HeaderValue>,
    pub(super) agent_type: Option<HeaderValue>,
    pub(super) prompt_id: Option<HeaderValue>,
    pub(super) prev_tool_durations: Option<HeaderValue>,
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

    /// Codex's headers: a thread is an agent, `x-openai-subagent` names the
    /// subagent kind and gives the request class.
    pub(crate) fn from_codex_headers(headers: &HeaderMap) -> Self {
        let subagent = headers.get(CODEX_SUBAGENT);
        let is_codex = headers
            .get(ORIGINATOR)
            .is_some_and(|v| v.as_bytes().starts_with(b"codex"));
        Self {
            request_class: RequestClass::from_codex_subagent(
                subagent.map(HeaderValue::as_bytes),
                is_codex,
            ),
            agent_id: headers.get(CODEX_THREAD_ID).cloned(),
            parent_agent_id: headers.get(CODEX_PARENT_THREAD_ID).cloned(),
            agent_type: subagent.cloned(),
            ..Self::default()
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
