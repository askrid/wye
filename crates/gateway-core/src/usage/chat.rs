//! Token usage extraction from an OpenAI Chat Completions SSE stream.
//!
//! Chat streams carry only `data:` lines, one chunk each, ending with
//! `data: [DONE]`. Usage arrives in a final chunk with empty `choices`, and
//! only when the client sets `stream_options.include_usage`; Wye does not
//! add it (no rewrites), so a stream without it reports `unknown`. Lines
//! are found with SIMD `memchr` and parsed only when a substring search
//! shows they matter: the first chunk (the model), a chunk naming
//! `"usage":{`, or one starting a tool call (`"tool_calls"` with a `"name"`
//! key; inside argument strings quotes are escaped, so they never match).
//! Other chunks, the content and argument deltas, are only searched.

use std::sync::LazyLock;

use memchr::memmem;
use serde::Deserialize;

use super::{
    Lines, Scanned, ToolCalls, Usage, UsageStatus, count_tool, openai::ChatUsage, record_model,
};

/// Longest data line kept when it continues into the next chunk; longer
/// lines are content deltas, except a usage chunk, which then marks usage
/// unknown.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// The substring searches, built once per process.
static USAGE_KEY: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(br#""usage":{"#));
static TOOL_CALLS_KEY: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(br#""tool_calls""#));
static NAME_KEY: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(br#""name":""#));

/// Incremental usage scanner for one Chat Completions stream.
#[derive(Debug, Default)]
pub struct ChatUsageScanner {
    lines: Lines,
    usage: Usage,
    model: Option<Box<str>>,
    tools: Option<Box<ToolCalls>>,
    saw_usage: bool,
    /// A data line was seen: the model is read from the first.
    saw_data: bool,
    corrupt: bool,
}

impl ChatUsageScanner {
    /// For a stream that has not started.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the next chunk of the stream, split anywhere.
    pub fn feed(&mut self, chunk: &[u8]) {
        let mut lines = std::mem::take(&mut self.lines);
        lines.feed(chunk, self, |_| MAX_LINE_BYTES, Self::process_line);
        self.lines = lines;
    }

    fn process_line(&mut self, line: &[u8], truncated: bool) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = data.trim_ascii();
        let first = !std::mem::replace(&mut self.saw_data, true);
        let has_usage = USAGE_KEY.find(data).is_some();
        if truncated {
            // The usage chunk is small; a cut one cannot be read.
            self.corrupt |= has_usage;
            return;
        }
        if has_usage {
            match serde_json::from_slice::<UsageChunk<'_>>(data) {
                Ok(chunk) => {
                    record_model(&mut self.model, chunk.model);
                    if super::unpriced_tier(chunk.service_tier) {
                        self.usage.unpriced = true;
                    }
                    if let Some(usage) = chunk.usage {
                        self.saw_usage = true;
                        usage.merge_into(&mut self.usage);
                    }
                }
                Err(_) => self.corrupt = true,
            }
        } else if first && let Ok(chunk) = serde_json::from_slice::<ModelChunk<'_>>(data) {
            record_model(&mut self.model, chunk.model);
        }
        if TOOL_CALLS_KEY.find(data).is_some()
            && NAME_KEY.find(data).is_some()
            && let Ok(chunk) = serde_json::from_slice::<ToolChunk<'_>>(data)
        {
            // A tool call's first delta names its function; later deltas
            // carry only arguments.
            for name in chunk
                .choices
                .iter()
                .flat_map(|c| c.delta.tool_calls.iter().flatten())
                .filter_map(|call| call.function.as_ref()?.name)
            {
                count_tool(&mut self.tools, name);
            }
        }
    }

    /// The counts reported so far.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The model the provider reported serving.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Complete once the usage chunk arrived; without `include_usage` there
    /// is none, so the stream is unknown.
    pub fn status(&self) -> UsageStatus {
        if self.corrupt || !self.saw_usage {
            UsageStatus::Unknown
        } else {
            UsageStatus::Complete
        }
    }

    /// What the scan saw, once the stream has ended.
    pub fn finish(self) -> Scanned {
        Scanned {
            status: self.status(),
            usage: self.usage,
            model: self.model,
            tool_calls: self.tools,
        }
    }
}

#[derive(Deserialize)]
struct UsageChunk<'a> {
    #[serde(borrow)]
    model: Option<&'a str>,
    #[serde(borrow)]
    service_tier: Option<&'a str>,
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ModelChunk<'a> {
    #[serde(borrow)]
    model: Option<&'a str>,
}

#[derive(Deserialize)]
struct ToolChunk<'a> {
    #[serde(borrow, default)]
    choices: Vec<ToolChoice<'a>>,
}

#[derive(Deserialize)]
struct ToolChoice<'a> {
    #[serde(borrow)]
    delta: ToolDelta<'a>,
}

#[derive(Deserialize)]
struct ToolDelta<'a> {
    #[serde(borrow)]
    tool_calls: Option<Vec<ToolCall<'a>>>,
}

#[derive(Deserialize)]
struct ToolCall<'a> {
    #[serde(borrow)]
    function: Option<Function<'a>>,
}

#[derive(Deserialize)]
struct Function<'a> {
    #[serde(borrow)]
    name: Option<&'a str>,
}

#[cfg(test)]
mod tests;
