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

use memchr::memmem;
use serde::Deserialize;

use super::{
    ToolCalls, Usage, UsageStatus, count_tool,
    fields::{InputDetails, merge_openai},
};

/// Longest data line kept when it continues into the next chunk; longer
/// lines are content deltas, except a usage chunk, which then marks usage
/// unknown.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// Incremental usage scanner for one Chat Completions stream.
#[derive(Debug)]
pub struct ChatUsageScanner {
    /// Substring searchers, built once per stream rather than per line.
    usage_key: memmem::Finder<'static>,
    tool_calls_key: memmem::Finder<'static>,
    name_key: memmem::Finder<'static>,
    /// Partial line carried over from the previous chunk.
    line: Vec<u8>,
    line_truncated: bool,
    usage: Usage,
    model: Option<Box<str>>,
    tools: Option<Box<ToolCalls>>,
    saw_usage: bool,
    /// A data line was seen: the model is read from the first.
    saw_data: bool,
    corrupt: bool,
}

impl Default for ChatUsageScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatUsageScanner {
    pub fn new() -> Self {
        Self {
            usage_key: memmem::Finder::new(br#""usage":{"#),
            tool_calls_key: memmem::Finder::new(br#""tool_calls""#),
            name_key: memmem::Finder::new(br#""name":""#),
            line: Vec::new(),
            line_truncated: false,
            usage: Usage::default(),
            model: None,
            tools: None,
            saw_usage: false,
            saw_data: false,
            corrupt: false,
        }
    }

    pub fn feed(&mut self, mut chunk: &[u8]) {
        while let Some(newline) = memchr::memchr(b'\n', chunk) {
            let (segment, rest) = chunk.split_at(newline);
            chunk = rest.get(1..).unwrap_or_default();
            if self.line.is_empty() && !self.line_truncated {
                // Common case: the whole line is in this chunk; no copy.
                self.process_line(segment, false);
            } else {
                self.carry(segment);
                let line = std::mem::take(&mut self.line);
                let truncated = std::mem::replace(&mut self.line_truncated, false);
                self.process_line(&line, truncated);
                self.line = line;
                self.line.clear();
            }
        }
        if !chunk.is_empty() {
            self.carry(chunk);
        }
    }

    fn carry(&mut self, bytes: &[u8]) {
        let room = MAX_LINE_BYTES.saturating_sub(self.line.len());
        if bytes.len() > room {
            self.line_truncated = true;
        }
        self.line
            .extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
    }

    fn process_line(&mut self, line: &[u8], truncated: bool) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = data.trim_ascii();
        let first = !std::mem::replace(&mut self.saw_data, true);
        let has_usage = self.usage_key.find(data).is_some();
        if truncated {
            // The usage chunk is small; a cut one cannot be read.
            self.corrupt |= has_usage;
            return;
        }
        if has_usage {
            match serde_json::from_slice::<UsageChunk<'_>>(data) {
                Ok(chunk) => {
                    if let Some(model) = chunk.model {
                        self.model = Some(model.into());
                    }
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
            self.model = chunk.model.map(Box::from);
        }
        if self.tool_calls_key.find(data).is_some()
            && self.name_key.find(data).is_some()
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

    pub fn usage(&self) -> Usage {
        self.usage
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn take_tool_calls(&mut self) -> Option<Box<ToolCalls>> {
        self.tools.take()
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
}

/// `usage`, as Chat Completions reports it.
#[derive(Deserialize)]
pub(super) struct ChatUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    prompt_tokens_details: Option<InputDetails>,
}

impl ChatUsage {
    /// Reasoning tokens are part of `completion_tokens`, as billed.
    pub(super) fn merge_into(self, usage: &mut Usage) {
        merge_openai(
            usage,
            self.prompt_tokens,
            self.completion_tokens,
            self.prompt_tokens_details,
        );
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
