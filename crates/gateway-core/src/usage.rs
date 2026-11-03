//! Token usage extraction from responses: Anthropic Messages streams in
//! `messages`, OpenAI Responses streams in `responses`, Chat Completions
//! streams in `chat`, and non-streaming JSON bodies of all three in `json`;
//! [`UsageScanner`] picks one per API and content type. HTTP chunk boundaries
//! are not SSE boundaries: the stream scanners take lines split anywhere,
//! including inside UTF-8.

mod chat;
mod fields;
mod json;
mod messages;
mod raw;
mod responses;

pub use chat::ChatUsageScanner;
pub use json::JsonUsageScanner;
pub use messages::MessagesUsageScanner;
pub use responses::ResponsesUsageScanner;

use crate::api::Api;

/// Longest usage-bearing data line kept. Larger events mark usage unknown
/// instead of growing memory.
pub const MAX_USAGE_LINE_BYTES: usize = 64 * 1024;
/// Bytes kept from lines of other events: enough to recognize `event:` lines.
const CLASSIFY_PREFIX_BYTES: usize = 32;
/// Longest `content_block_start` data line read; a `tool_use` start carries
/// an empty `input`, so longer lines are other block types and are skipped.
const MAX_BLOCK_START_LINE_BYTES: usize = 4 * 1024;
/// Distinct tool names counted per response; further ones count as overflow.
pub const MAX_TOOL_NAMES: usize = 8;
/// Longest tool name counted by name.
pub const MAX_TOOL_NAME_BYTES: usize = 64;
/// Longest model name kept from a response. Every record, OTLP batch, and
/// metric series carries it, so a longer one is dropped.
pub const MAX_RECORDED_MODEL_BYTES: usize = 128;

/// `model` when it fits [`MAX_RECORDED_MODEL_BYTES`].
fn kept_model<M: AsRef<str>>(model: Option<M>) -> Option<M> {
    model.filter(|m| m.as_ref().len() <= MAX_RECORDED_MODEL_BYTES)
}

/// Whether an OpenAI `service_tier` is billed outside the token prices:
/// any tier but `default` makes the cost partial.
fn unpriced_tier(tier: Option<&str>) -> bool {
    tier.is_some_and(|tier| tier != "default")
}

/// Token counts reported by the provider. `None` when the response did not report the field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    /// Cache writes by TTL (`usage.cache_creation`), when reported.
    pub cache_creation_5m_tokens: Option<u64>,
    pub cache_creation_1h_tokens: Option<u64>,
    /// The response used something billed outside token prices: per-use
    /// server tools (`server_tool_use`) or OpenAI's hosted tools, or a
    /// service tier or speed other than standard.
    pub unpriced: bool,
}

/// How much of the usage a scan saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageStatus {
    /// The provider's final usage report arrived (Messages `message_delta`,
    /// a Responses terminal event, a Chat usage chunk, a JSON body's `usage`).
    Complete,
    /// A Messages stream ended after `message_start`: output tokens are a lower bound.
    Partial,
    /// No usable usage event, or a usage event was malformed or oversized.
    Unknown,
}

impl UsageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
        }
    }
}

/// `tool_use` blocks of one response, by tool name. Fixed size, so counting
/// never allocates after the first tool call boxes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCalls {
    names: [([u8; MAX_TOOL_NAME_BYTES], u8, u32); MAX_TOOL_NAMES],
    len: usize,
    /// Calls whose name was too long, not a plain token, or beyond
    /// [`MAX_TOOL_NAMES`] distinct names.
    other: u32,
}

impl ToolCalls {
    fn new() -> Self {
        Self {
            names: [([0; MAX_TOOL_NAME_BYTES], 0, 0); MAX_TOOL_NAMES],
            len: 0,
            other: 0,
        }
    }

    fn count(&mut self, name: &str) {
        if crate::agent::label(name.as_bytes()).is_none() || name.len() > MAX_TOOL_NAME_BYTES {
            self.other = self.other.saturating_add(1);
            return;
        }
        let used = self.names.get_mut(..self.len).unwrap_or_default();
        if let Some(entry) = used
            .iter_mut()
            .find(|(bytes, len, _)| bytes.get(..usize::from(*len)) == Some(name.as_bytes()))
        {
            entry.2 = entry.2.saturating_add(1);
        } else if let Some(entry) = self.names.get_mut(self.len) {
            if let Some(dst) = entry.0.get_mut(..name.len()) {
                dst.copy_from_slice(name.as_bytes());
            }
            // The length check above bounds it to 64.
            entry.1 = name.len() as u8;
            entry.2 = 1;
            self.len += 1;
        } else {
            self.other = self.other.saturating_add(1);
        }
    }

    /// Tool names and call counts, in first-call order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u32)> {
        self.names
            .get(..self.len)
            .unwrap_or_default()
            .iter()
            .filter_map(|(bytes, len, count)| {
                let name = std::str::from_utf8(bytes.get(..usize::from(*len))?).ok()?;
                Some((name, *count))
            })
    }

    /// Calls not counted by name.
    pub fn other(&self) -> u32 {
        self.other
    }
}

/// Counts a call of the tool `name`, boxing the table on the first.
fn count_tool(tools: &mut Option<Box<ToolCalls>>, name: &str) {
    tools
        .get_or_insert_with(|| Box::new(ToolCalls::new()))
        .count(name);
}

/// The usage scanner for one response body: an SSE stream of any of the APIs,
/// or a non-streaming JSON body. An enum, so the per-chunk call is a match,
/// not a virtual call; the other scanners are boxed, so Messages streams
/// keep their size.
#[derive(Debug)]
pub enum UsageScanner {
    Messages(MessagesUsageScanner),
    Responses(Box<ResponsesUsageScanner>),
    Chat(Box<ChatUsageScanner>),
    Json(Box<JsonUsageScanner>),
}

impl UsageScanner {
    /// For an SSE stream of `api`.
    pub fn new(api: Api) -> Self {
        match api {
            Api::Messages => Self::Messages(MessagesUsageScanner::new()),
            Api::Responses => Self::Responses(Box::default()),
            Api::ChatCompletions => Self::Chat(Box::default()),
        }
    }

    /// For a non-streaming JSON body of `api`.
    pub fn json(api: Api) -> Self {
        Self::Json(Box::new(JsonUsageScanner::new(api)))
    }

    /// Feeds the next chunk of response bytes.
    pub fn feed(&mut self, chunk: &[u8]) {
        match self {
            Self::Messages(s) => s.feed(chunk),
            Self::Responses(s) => s.feed(chunk),
            Self::Chat(s) => s.feed(chunk),
            Self::Json(s) => s.feed(chunk),
        }
    }

    pub fn usage(&self) -> Usage {
        match self {
            Self::Messages(s) => s.usage(),
            Self::Responses(s) => s.usage(),
            Self::Chat(s) => s.usage(),
            Self::Json(s) => s.usage(),
        }
    }

    pub fn model(&self) -> Option<&str> {
        match self {
            Self::Messages(s) => s.model(),
            Self::Responses(s) => s.model(),
            Self::Chat(s) => s.model(),
            Self::Json(s) => s.model(),
        }
    }

    pub fn status(&self) -> UsageStatus {
        match self {
            Self::Messages(s) => s.status(),
            Self::Responses(s) => s.status(),
            Self::Chat(s) => s.status(),
            Self::Json(s) => s.status(),
        }
    }

    /// Moves the tool calls out, keeping their allocation.
    pub fn take_tool_calls(&mut self) -> Option<Box<ToolCalls>> {
        match self {
            Self::Messages(s) => s.take_tool_calls(),
            Self::Responses(s) => s.take_tool_calls(),
            Self::Chat(s) => s.take_tool_calls(),
            Self::Json(s) => s.take_tool_calls(),
        }
    }
}

/// Cuts a stream's chunks into lines, carrying a partial line to the next chunk.
#[derive(Debug, Default)]
struct Lines {
    /// Partial line carried over from the previous chunk.
    line: Vec<u8>,
    /// The carried line was longer than the bytes kept.
    truncated: bool,
}

impl Lines {
    /// Calls `process` on each line that ends in `chunk`, with whether it was
    /// truncated; a partial line keeps at most `limit` bytes. Takes the
    /// scanner apart from its `Lines`, so `process` can borrow it mutably.
    #[inline(always)]
    fn feed<S>(
        &mut self,
        mut chunk: &[u8],
        scanner: &mut S,
        limit: impl Fn(&S) -> usize,
        mut process: impl FnMut(&mut S, &[u8], bool),
    ) {
        // SIMD search: every streamed byte passes through here.
        while let Some(newline) = memchr::memchr(b'\n', chunk) {
            let (segment, rest) = chunk.split_at(newline);
            chunk = rest.get(1..).unwrap_or_default();
            if self.line.is_empty() && !self.truncated {
                // Common case: the whole line is in this chunk; no copy.
                process(scanner, segment, false);
            } else {
                self.carry(segment, limit(scanner));
                process(scanner, &self.line, self.truncated);
                // Keeps the allocation for the next carried line.
                self.line.clear();
                self.truncated = false;
            }
        }
        if !chunk.is_empty() {
            self.carry(chunk, limit(scanner));
        }
    }

    fn carry(&mut self, bytes: &[u8], limit: usize) {
        let room = limit.saturating_sub(self.line.len());
        if bytes.len() > room {
            self.truncated = true;
        }
        self.line
            .extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests;
