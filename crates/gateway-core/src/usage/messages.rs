//! The usage scanner for Anthropic Messages streams. It reads the forwarded
//! bytes without changing, delaying, or copying them: it keeps at most one
//! partial line, parses JSON only for the two events that carry usage
//! (`message_start` and `message_delta`) and for `content_block_start` (once
//! per content block, to count `tool_use` blocks by tool name), and skips
//! every other event after reading its line prefix.

use super::{
    CLASSIFY_PREFIX_BYTES, Lines, MAX_BLOCK_START_LINE_BYTES, MAX_USAGE_LINE_BYTES, ToolCalls,
    Usage, UsageStatus, count_tool, kept_model,
    raw::{BlockStartData, DeltaData, StartData},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Event {
    #[default]
    Other,
    MessageStart,
    MessageDelta,
    ContentBlockStart,
}

/// Incremental usage scanner for one Anthropic Messages stream.
#[derive(Debug, Default)]
pub struct MessagesUsageScanner {
    pub(super) lines: Lines,
    event: Event,
    usage: Usage,
    model: Option<Box<str>>,
    /// Boxed on the first `tool_use` block: responses without tool calls
    /// stay small.
    pub(super) tools: Option<Box<ToolCalls>>,
    saw_start: bool,
    saw_delta: bool,
    /// A usage event could not be read; the counts are not trustworthy.
    corrupt: bool,
}

impl MessagesUsageScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        let mut lines = std::mem::take(&mut self.lines);
        lines.feed(chunk, self, Self::line_limit, Self::process_line);
        self.lines = lines;
    }

    /// Bytes kept of a partial line: as much as the current event needs.
    fn line_limit(&self) -> usize {
        match self.event {
            Event::Other => CLASSIFY_PREFIX_BYTES,
            Event::MessageStart | Event::MessageDelta => MAX_USAGE_LINE_BYTES,
            Event::ContentBlockStart => MAX_BLOCK_START_LINE_BYTES,
        }
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
                    self.model = kept_model(start.message.model).map(String::into_boxed_str);
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
                    count_tool(
                        &mut self.tools,
                        start.content_block.name.unwrap_or_default(),
                    );
                }
            }
            Event::Other => {}
        }
    }

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
