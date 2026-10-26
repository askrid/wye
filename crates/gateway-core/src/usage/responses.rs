//! Token usage extraction from an OpenAI Responses SSE stream.
//!
//! Usage arrives once, in the terminal event (`response.completed`,
//! `response.incomplete`, or `response.failed`), whose data line carries
//! the whole response, output included, so it can be megabytes long. That
//! line is lexed as it streams, with constant memory: only `response.model`,
//! `response.service_tier`, and the small `response.usage` object are kept.
//! `response.output_item.added` lines, small because an item starts without
//! its content, are parsed to count tool calls. Every other event is skipped
//! after its `event:` line.
//!
//! OpenAI's `input_tokens` include cached tokens; [`Usage::input_tokens`]
//! does not (Anthropic's meaning), so the cached share moves to
//! [`Usage::cache_read_input_tokens`] and records mean the same across APIs.

use serde::Deserialize;

use super::{ToolCalls, Usage, UsageStatus};
use crate::json::{Nesting, StringEnd, is_space};

/// Longest event name kept; longer names are other events.
const EVENT_NAME_BYTES: usize = 64;
/// Longest `response.output_item.added` data line read.
const MAX_ITEM_LINE_BYTES: usize = 4 * 1024;
/// Longest `response.usage` object kept; larger marks usage unknown.
const MAX_USAGE_BYTES: usize = 2 * 1024;
/// Longest `model` or `service_tier` kept.
const MAX_FIELD_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Other,
    /// `response.created`: the model, before any output.
    Created,
    /// `response.completed`, `response.incomplete`, `response.failed`.
    Terminal,
    ItemAdded,
}

/// Where the scanner is within the current SSE line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    /// Reading the field name, up to `event:` or `data:`.
    Prefix,
    EventName,
    Data,
    Skip,
}

/// Incremental usage scanner for one Responses stream.
#[derive(Debug)]
pub struct ResponsesUsageScanner {
    line: Line,
    prefix: [u8; 6],
    prefix_len: usize,
    name: [u8; EVENT_NAME_BYTES],
    name_len: usize,
    name_long: bool,
    event: Event,
    /// The current `response.output_item.added` data line.
    item: Vec<u8>,
    item_long: bool,
    fields: ResponseFields,
    usage: Usage,
    model: Option<Box<str>>,
    tools: Option<Box<ToolCalls>>,
    saw_usage: bool,
    corrupt: bool,
}

impl Default for ResponsesUsageScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesUsageScanner {
    pub fn new() -> Self {
        Self {
            line: Line::Prefix,
            prefix: [0; 6],
            prefix_len: 0,
            name: [0; EVENT_NAME_BYTES],
            name_len: 0,
            name_long: false,
            event: Event::Other,
            item: Vec::new(),
            item_long: false,
            fields: ResponseFields::new(),
            usage: Usage::default(),
            model: None,
            tools: None,
            saw_usage: false,
            corrupt: false,
        }
    }

    /// Feeds the next chunk of response bytes.
    pub fn feed(&mut self, mut chunk: &[u8]) {
        while !chunk.is_empty() {
            let newline = memchr::memchr(b'\n', chunk);
            let segment = chunk
                .get(..newline.unwrap_or(chunk.len()))
                .unwrap_or_default();
            self.segment(segment);
            match newline {
                Some(at) => {
                    self.end_line();
                    chunk = chunk.get(at + 1..).unwrap_or_default();
                }
                None => break,
            }
        }
    }

    /// Part of a line, without its newline.
    fn segment(&mut self, mut bytes: &[u8]) {
        // The field name: at most six bytes per line, read one at a time.
        while self.line == Line::Prefix {
            let Some((&byte, rest)) = bytes.split_first() else {
                return;
            };
            bytes = rest;
            if let Some(slot) = self.prefix.get_mut(self.prefix_len) {
                *slot = byte;
                self.prefix_len += 1;
            }
            let prefix = self.prefix.get(..self.prefix_len).unwrap_or_default();
            self.line = if prefix == b"event:" {
                Line::EventName
            } else if prefix == b"data:" {
                Line::Data
            } else if b"event:".starts_with(prefix) || b"data:".starts_with(prefix) {
                Line::Prefix
            } else {
                Line::Skip
            };
        }
        match self.line {
            Line::EventName => {
                let room = EVENT_NAME_BYTES - self.name_len;
                if bytes.len() > room {
                    self.name_long = true;
                }
                let take = room.min(bytes.len());
                if let Some(dst) = self.name.get_mut(self.name_len..self.name_len + take) {
                    dst.copy_from_slice(bytes.get(..take).unwrap_or_default());
                }
                self.name_len += take;
            }
            Line::Data => self.data(bytes),
            Line::Prefix | Line::Skip => {}
        }
    }

    /// Bytes of a data line's value.
    fn data(&mut self, bytes: &[u8]) {
        match self.event {
            Event::Created | Event::Terminal => self.fields.feed(bytes),
            Event::ItemAdded => {
                let room = MAX_ITEM_LINE_BYTES.saturating_sub(self.item.len());
                if bytes.len() > room {
                    self.item_long = true;
                } else {
                    self.item.extend_from_slice(bytes);
                }
            }
            Event::Other => {}
        }
    }

    fn end_line(&mut self) {
        let prefix = self.prefix.get(..self.prefix_len).unwrap_or_default();
        if prefix.is_empty() || prefix == b"\r" {
            // A blank line ends the event.
            self.event = Event::Other;
        }
        match self.line {
            Line::EventName => {
                let name = self.name.get(..self.name_len).unwrap_or_default();
                self.event = match name.trim_ascii() {
                    _ if self.name_long => Event::Other,
                    b"response.created" => Event::Created,
                    b"response.completed" | b"response.incomplete" | b"response.failed" => {
                        Event::Terminal
                    }
                    b"response.output_item.added" => Event::ItemAdded,
                    _ => Event::Other,
                };
            }
            Line::Data => self.end_data(),
            Line::Prefix | Line::Skip => {}
        }
        self.line = Line::Prefix;
        self.prefix_len = 0;
        self.name_len = 0;
        self.name_long = false;
    }

    fn end_data(&mut self) {
        match self.event {
            Event::Created | Event::Terminal => {
                let fields = std::mem::replace(&mut self.fields, ResponseFields::new());
                self.read_fields(&fields);
            }
            Event::ItemAdded => {
                if !self.item_long {
                    self.read_item();
                }
                self.item.clear();
                self.item_long = false;
            }
            Event::Other => {}
        }
    }

    fn read_fields(&mut self, fields: &ResponseFields) {
        if fields.failed {
            self.corrupt = true;
            return;
        }
        if let Some(model) = fields.model() {
            self.model = Some(model.into());
        }
        if self.event != Event::Terminal {
            return;
        }
        // The tier that served the request; others than default are priced
        // differently.
        if fields.service_tier().is_some_and(|tier| tier != "default") {
            self.usage.unpriced = true;
        }
        match fields.usage() {
            Err(()) => self.corrupt = true,
            Ok(None) => {}
            Ok(Some(raw)) => {
                self.saw_usage = true;
                let cached = raw
                    .input_tokens_details
                    .and_then(|d| d.cached_tokens)
                    .unwrap_or(0);
                self.usage.input_tokens = raw.input_tokens.map(|i| i.saturating_sub(cached));
                self.usage.cache_read_input_tokens = raw.input_tokens.map(|_| cached);
                self.usage.output_tokens = raw.output_tokens;
            }
        }
    }

    fn read_item(&mut self) {
        let Ok(added) = serde_json::from_slice::<ItemAdded<'_>>(self.item.trim_ascii()) else {
            return;
        };
        let item = added.item;
        let name = match item.kind {
            "function_call" | "custom_tool_call" => item.name.unwrap_or_default(),
            "message" | "reasoning" => return,
            kind => match kind.strip_suffix("_call") {
                Some(tool) => {
                    // Hosted tools are billed per call, outside token prices.
                    if matches!(
                        tool,
                        "web_search" | "file_search" | "code_interpreter" | "image_generation"
                    ) {
                        self.usage.unpriced = true;
                    }
                    tool
                }
                None => return,
            },
        };
        self.tools
            .get_or_insert_with(|| Box::new(ToolCalls::new()))
            .count(name);
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The model the provider reported.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Tool calls the response started, by name; `None` without any.
    pub fn tool_calls(&self) -> Option<&ToolCalls> {
        self.tools.as_deref()
    }

    /// Moves the tool calls out, keeping their allocation.
    pub fn take_tool_calls(&mut self) -> Option<Box<ToolCalls>> {
        self.tools.take()
    }

    /// Complete once a terminal event reported usage; there are no
    /// interim counts, so a stream cut before it is unknown.
    pub fn status(&self) -> UsageStatus {
        if self.corrupt || !self.saw_usage {
            UsageStatus::Unknown
        } else {
            UsageStatus::Complete
        }
    }
}

#[derive(Deserialize)]
struct ItemAdded<'a> {
    #[serde(borrow)]
    item: Item<'a>,
}

#[derive(Deserialize)]
struct Item<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(borrow)]
    name: Option<&'a str>,
}

/// `response.usage`, as OpenAI reports it.
#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    input_tokens_details: Option<InputDetails>,
}

#[derive(Deserialize)]
struct InputDetails {
    cached_tokens: Option<u64>,
}

/// The `response` member keys read from a data line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Other,
    Model,
    ServiceTier,
    Usage,
}

/// A constant-memory lexer for one data line, keeping `response.model`,
/// `response.service_tier`, and the raw `response.usage` object.
#[derive(Debug)]
struct ResponseFields {
    nest: Nesting,
    started: bool,
    /// Not a JSON object, or brackets that do not match.
    failed: bool,
    in_string: bool,
    string: StringEnd,
    expect_key: bool,
    /// The depth-1 key being read is `response`.
    in_response: bool,
    /// The depth-2 key (a member of `response`) whose value comes next.
    field: Field,
    /// The current string is kept: a depth 1-2 key or a field's value.
    capturing: bool,
    is_key: bool,
    text: [u8; MAX_FIELD_BYTES],
    text_len: usize,
    text_long: bool,
    model: ([u8; MAX_FIELD_BYTES], usize),
    service_tier: ([u8; MAX_FIELD_BYTES], usize),
    /// The usage object's bytes; empty when absent or `null`.
    usage: Vec<u8>,
    /// Inside the usage object.
    in_usage: bool,
    usage_long: bool,
}

impl ResponseFields {
    fn new() -> Self {
        Self {
            nest: Nesting::default(),
            started: false,
            failed: false,
            in_string: false,
            string: StringEnd::default(),
            expect_key: false,
            in_response: false,
            field: Field::Other,
            capturing: false,
            is_key: false,
            text: [0; MAX_FIELD_BYTES],
            text_len: 0,
            text_long: false,
            model: ([0; MAX_FIELD_BYTES], 0),
            service_tier: ([0; MAX_FIELD_BYTES], 0),
            usage: Vec::new(),
            in_usage: false,
            usage_long: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        // Where the usage bytes of this chunk start, while inside usage.
        let mut usage_from = self.in_usage.then_some(0);
        let mut i = 0;
        while i < chunk.len() && !self.failed {
            if self.in_string {
                let rest = chunk.get(i..).unwrap_or_default();
                let end = self.string.find(rest);
                if self.capturing {
                    self.keep(rest.get(..end.unwrap_or(rest.len())).unwrap_or_default());
                }
                match end {
                    Some(quote) => {
                        self.in_string = false;
                        if self.capturing {
                            self.end_string();
                        }
                        self.expect_key = false;
                        i += quote + 1;
                    }
                    None => i = chunk.len(),
                }
                continue;
            }
            let Some(&byte) = chunk.get(i) else { break };
            if is_space(byte) {
                i += 1;
                continue;
            }
            if !self.started {
                self.started = true;
                if byte != b'{' {
                    self.failed = true;
                    break;
                }
            }
            match byte {
                b'"' => {
                    self.is_key = self.expect_key;
                    let depth = self.nest.depth();
                    self.capturing = if self.is_key {
                        depth == 1 || (depth == 2 && self.in_response)
                    } else {
                        depth == 2 && matches!(self.field, Field::Model | Field::ServiceTier)
                    };
                    self.text_len = 0;
                    self.text_long = false;
                    self.in_string = true;
                    self.string.start();
                }
                b'{' | b'[' => {
                    if self.nest.depth() == 2 && self.field == Field::Usage && byte == b'{' {
                        self.in_usage = true;
                        usage_from = Some(i);
                    }
                    self.nest.open(byte == b'[');
                    self.expect_key = byte == b'{';
                }
                b'}' | b']' => {
                    if !self.nest.close(byte == b']') {
                        self.failed = true;
                        break;
                    }
                    let depth = self.nest.depth();
                    if self.in_usage && depth == 2 {
                        self.in_usage = false;
                        if let Some(from) = usage_from.take() {
                            self.keep_usage(chunk.get(from..=i).unwrap_or_default());
                        }
                    }
                    if depth <= 1 {
                        self.field = Field::Other;
                    }
                    self.expect_key = false;
                }
                b',' => {
                    let depth = self.nest.depth();
                    self.expect_key = depth > 0 && !self.nest.is_array(depth);
                    if depth <= 2 {
                        self.field = Field::Other;
                    }
                    if depth == 1 {
                        self.in_response = false;
                    }
                }
                b':' => self.expect_key = false,
                _ => {}
            }
            i += 1;
        }
        if self.in_usage
            && let Some(from) = usage_from
        {
            self.keep_usage(chunk.get(from..).unwrap_or_default());
        }
    }

    fn keep(&mut self, content: &[u8]) {
        if content.contains(&b'\\') {
            // An escaped key or value matches nothing read here.
            self.text_long = true;
        }
        match self
            .text
            .get_mut(self.text_len..self.text_len + content.len())
        {
            Some(slot) => {
                slot.copy_from_slice(content);
                self.text_len += content.len();
            }
            None => self.text_long = true,
        }
    }

    fn keep_usage(&mut self, bytes: &[u8]) {
        if self.usage.len() + bytes.len() > MAX_USAGE_BYTES {
            self.usage_long = true;
        } else {
            self.usage.extend_from_slice(bytes);
        }
    }

    fn end_string(&mut self) {
        let text: &[u8] = if self.text_long {
            b""
        } else {
            self.text.get(..self.text_len).unwrap_or_default()
        };
        let depth = self.nest.depth();
        if self.is_key {
            if depth == 1 {
                self.in_response = text == b"response";
            } else {
                self.field = match text {
                    b"model" => Field::Model,
                    b"service_tier" => Field::ServiceTier,
                    b"usage" => Field::Usage,
                    _ => Field::Other,
                };
            }
            return;
        }
        let slot = match self.field {
            Field::Model => &mut self.model,
            Field::ServiceTier => &mut self.service_tier,
            Field::Other | Field::Usage => return,
        };
        if let Some(dst) = slot.0.get_mut(..text.len()) {
            dst.copy_from_slice(text);
            slot.1 = text.len();
        }
    }

    fn model(&self) -> Option<&str> {
        let (bytes, len) = &self.model;
        std::str::from_utf8(bytes.get(..*len)?)
            .ok()
            .filter(|m| !m.is_empty())
    }

    fn service_tier(&self) -> Option<&str> {
        let (bytes, len) = &self.service_tier;
        std::str::from_utf8(bytes.get(..*len)?)
            .ok()
            .filter(|t| !t.is_empty())
    }

    /// `Ok(None)` when absent or `null`; `Err` when too large or malformed.
    fn usage(&self) -> Result<Option<RawUsage>, ()> {
        if self.usage_long || self.in_usage {
            return Err(());
        }
        if self.usage.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&self.usage)
            .map(Some)
            .map_err(|_| ())
    }
}

#[cfg(test)]
mod tests;
