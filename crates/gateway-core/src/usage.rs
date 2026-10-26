//! Token usage extraction from an Anthropic Messages SSE stream.
//!
//! [`SseUsageScanner`] reads the forwarded bytes without changing, delaying, or
//! copying them. It keeps at most one partial line, parses JSON only for the two
//! events that carry usage (`message_start` and `message_delta`) and for
//! `content_block_start` (once per content block, to count `tool_use` blocks by
//! tool name), and skips every other event after reading its line prefix.
//! HTTP chunk boundaries are not SSE boundaries: lines may be split anywhere,
//! including inside UTF-8.

use std::{borrow::Cow, fmt};

use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
};

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

/// Token counts reported by the provider. `None` when the stream did not report the field.
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
    /// server tools (`server_tool_use`), or a service tier or speed other
    /// than standard.
    pub unpriced: bool,
}

/// How much of the usage a scan saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageStatus {
    /// `message_delta` usage arrived: the counts are the provider's final report.
    Complete,
    /// Only `message_start` arrived (stream ended early): output tokens are a lower bound.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Other,
    MessageStart,
    MessageDelta,
    ContentBlockStart,
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
        if crate::agent::token(name.as_bytes()).is_none() || name.len() > MAX_TOOL_NAME_BYTES {
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
            // The token check above bounds the length to 64.
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

/// Incremental usage scanner for one response stream.
#[derive(Debug)]
pub struct SseUsageScanner {
    /// Partial line carried over from the previous chunk.
    line: Vec<u8>,
    /// The carried line was longer than the bytes kept.
    line_truncated: bool,
    event: Event,
    usage: Usage,
    model: Option<Box<str>>,
    /// Boxed on the first `tool_use` block: responses without tool calls
    /// stay small.
    tools: Option<Box<ToolCalls>>,
    saw_start: bool,
    saw_delta: bool,
    /// A usage event could not be read; the counts are not trustworthy.
    corrupt: bool,
}

impl Default for SseUsageScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl SseUsageScanner {
    pub fn new() -> Self {
        Self {
            line: Vec::new(),
            line_truncated: false,
            event: Event::Other,
            usage: Usage::default(),
            model: None,
            tools: None,
            saw_start: false,
            saw_delta: false,
            corrupt: false,
        }
    }

    /// Feeds the next chunk of response bytes.
    pub fn feed(&mut self, mut chunk: &[u8]) {
        // SIMD search: every streamed byte passes through here.
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
                // Keep the allocation for the next carried line.
                self.line = line;
                self.line.clear();
            }
        }
        if !chunk.is_empty() {
            self.carry(chunk);
        }
    }

    /// Appends part of a line, keeping only as much as the current event needs.
    fn carry(&mut self, bytes: &[u8]) {
        let limit = match self.event {
            Event::Other => CLASSIFY_PREFIX_BYTES,
            Event::MessageStart | Event::MessageDelta => MAX_USAGE_LINE_BYTES,
            Event::ContentBlockStart => MAX_BLOCK_START_LINE_BYTES,
        };
        let room = limit.saturating_sub(self.line.len());
        if bytes.len() > room {
            self.line_truncated = true;
        }
        self.line
            .extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
    }

    fn process_line(&mut self, line: &[u8], truncated: bool) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            self.event = Event::Other;
        } else if let Some(name) = line.strip_prefix(b"event:") {
            self.event = match name.trim_ascii() {
                b"message_start" => Event::MessageStart,
                b"message_delta" => Event::MessageDelta,
                b"content_block_start" => Event::ContentBlockStart,
                _ => Event::Other,
            };
        } else if let Some(data) = line.strip_prefix(b"data:") {
            if self.event == Event::Other {
                return;
            }
            if truncated {
                // An oversized block start is not a tool call; usage is not affected.
                self.corrupt |= self.event != Event::ContentBlockStart;
                return;
            }
            self.read_data(data.trim_ascii());
        }
    }

    fn read_data(&mut self, data: &[u8]) {
        match self.event {
            Event::MessageStart => match serde_json::from_slice::<StartData>(data) {
                Ok(start) => {
                    self.saw_start = true;
                    self.model = start.message.model.map(String::into_boxed_str);
                    if let Some(raw) = start.message.usage {
                        raw.merge_into(&mut self.usage);
                    }
                }
                Err(_) => self.corrupt = true,
            },
            Event::MessageDelta => match serde_json::from_slice::<DeltaData>(data) {
                Ok(delta) => {
                    if let Some(raw) = delta.usage {
                        self.saw_delta = true;
                        raw.merge_into(&mut self.usage);
                    }
                }
                Err(_) => self.corrupt = true,
            },
            // Borrowed parse: no allocation. A name with escapes fails to
            // borrow and is skipped, like any malformed block start.
            Event::ContentBlockStart => {
                if let Ok(start) = serde_json::from_slice::<BlockStartData<'_>>(data)
                    && start.content_block.kind == "tool_use"
                {
                    self.tools
                        .get_or_insert_with(|| Box::new(ToolCalls::new()))
                        .count(start.content_block.name.unwrap_or_default());
                }
            }
            Event::Other => {}
        }
    }

    /// `tool_use` blocks the response started, by tool name; `None` when
    /// it requested no tool calls.
    pub fn tool_calls(&self) -> Option<&ToolCalls> {
        self.tools.as_deref()
    }

    /// Moves the tool calls out, keeping their allocation.
    pub fn take_tool_calls(&mut self) -> Option<Box<ToolCalls>> {
        self.tools.take()
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Model the provider reported in `message_start`.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn status(&self) -> UsageStatus {
        match (self.corrupt, self.saw_start, self.saw_delta) {
            (true, _, _) | (false, false, false) => UsageStatus::Unknown,
            (false, _, true) => UsageStatus::Complete,
            (false, true, false) => UsageStatus::Partial,
        }
    }
}

#[derive(Deserialize)]
struct StartData<'a> {
    #[serde(borrow)]
    message: StartMessage<'a>,
}

#[derive(Deserialize)]
struct StartMessage<'a> {
    model: Option<String>,
    #[serde(borrow)]
    usage: Option<RawUsage<'a>>,
}

#[derive(Deserialize)]
struct BlockStartData<'a> {
    #[serde(borrow)]
    content_block: ContentBlock<'a>,
}

#[derive(Deserialize)]
struct ContentBlock<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    name: Option<&'a str>,
}

#[derive(Deserialize)]
struct DeltaData<'a> {
    #[serde(borrow)]
    usage: Option<RawUsage<'a>>,
}

#[derive(Deserialize)]
struct RawUsage<'a> {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation: Option<RawCacheCreation>,
    server_tool_use: Option<AnyCount>,
    #[serde(borrow)]
    service_tier: Option<Cow<'a, str>>,
    #[serde(borrow)]
    speed: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct RawCacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

/// Whether any value in an object is a positive count; other values are
/// skipped, so a new field cannot fail the usage read. Allocates nothing.
struct AnyCount(bool);

impl<'de> Deserialize<'de> for AnyCount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(AnyCountVisitor)
    }
}

/// A count, or zero for anything that is not a non-negative integer.
struct Count(u64);

impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_any(AnyCountVisitor)
            .map(|any| Count(u64::from(any.0)))
    }
}

struct AnyCountVisitor;

impl<'de> Visitor<'de> for AnyCountVisitor {
    type Value = AnyCount;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a count or an object of counts")
    }

    fn visit_u64<E>(self, n: u64) -> Result<AnyCount, E> {
        Ok(AnyCount(n > 0))
    }

    fn visit_i64<E>(self, n: i64) -> Result<AnyCount, E> {
        Ok(AnyCount(n > 0))
    }

    fn visit_f64<E>(self, _: f64) -> Result<AnyCount, E> {
        Ok(AnyCount(false))
    }

    fn visit_str<E>(self, _: &str) -> Result<AnyCount, E> {
        Ok(AnyCount(false))
    }

    fn visit_bool<E>(self, _: bool) -> Result<AnyCount, E> {
        Ok(AnyCount(false))
    }

    fn visit_unit<E>(self) -> Result<AnyCount, E> {
        Ok(AnyCount(false))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<AnyCount, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(AnyCount(false))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<AnyCount, A::Error> {
        let mut any = false;
        while let Some((_, Count(n))) = map.next_entry::<IgnoredAny, Count>()? {
            any |= n > 0;
        }
        Ok(AnyCount(any))
    }
}

impl RawUsage<'_> {
    /// Later events replace earlier counts; `message_delta` output tokens are cumulative.
    fn merge_into(self, usage: &mut Usage) {
        let take = |field: &mut Option<u64>, value: Option<u64>| {
            if value.is_some() {
                *field = value;
            }
        };
        take(&mut usage.input_tokens, self.input_tokens);
        take(&mut usage.output_tokens, self.output_tokens);
        take(
            &mut usage.cache_creation_input_tokens,
            self.cache_creation_input_tokens,
        );
        take(
            &mut usage.cache_read_input_tokens,
            self.cache_read_input_tokens,
        );
        if let Some(split) = self.cache_creation {
            take(
                &mut usage.cache_creation_5m_tokens,
                split.ephemeral_5m_input_tokens,
            );
            take(
                &mut usage.cache_creation_1h_tokens,
                split.ephemeral_1h_input_tokens,
            );
        }
        let non_standard = |v: &Option<Cow<'_, str>>| v.as_deref().is_some_and(|v| v != "standard");
        usage.unpriced |= self.server_tool_use.is_some_and(|tools| tools.0)
            || non_standard(&self.service_tier)
            || non_standard(&self.speed);
    }
}

#[cfg(test)]
mod tests;
