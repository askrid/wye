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

use super::{
    ToolCalls, Usage, UsageStatus,
    fields::{Items, ResponseFields, openai_tool},
};

/// Longest event name kept; longer names are other events.
const EVENT_NAME_BYTES: usize = 64;
/// Longest `response.output_item.added` data line read.
const MAX_ITEM_LINE_BYTES: usize = 4 * 1024;

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
            fields: ResponseFields::new(Some(b"response"), Items::None),
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
        // The field name. Usually the whole name is in this segment.
        if self.line == Line::Prefix && self.prefix_len == 0 {
            if let Some(rest) = bytes.strip_prefix(b"event:") {
                self.line = Line::EventName;
                self.prefix_len = 6;
                bytes = rest;
            } else if let Some(rest) = bytes.strip_prefix(b"data:") {
                self.line = Line::Data;
                self.prefix_len = 5;
                bytes = rest;
            }
        }
        // Otherwise at most six bytes per line, read one at a time.
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
                let fields = std::mem::replace(
                    &mut self.fields,
                    ResponseFields::new(Some(b"response"), Items::None),
                );
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
        // The tier that served the request; a tier other than default is
        // priced differently.
        if fields.service_tier().is_some_and(|tier| tier != "default") {
            self.usage.unpriced = true;
        }
        match fields.openai_usage() {
            Err(()) => self.corrupt = true,
            Ok(None) => {}
            Ok(Some(raw)) => {
                self.saw_usage = true;
                raw.merge_into(&mut self.usage);
            }
        }
    }

    fn read_item(&mut self) {
        let Ok(added) = serde_json::from_slice::<ItemAdded<'_>>(self.item.trim_ascii()) else {
            return;
        };
        let item = added.item;
        let Some((name, hosted)) = openai_tool(item.kind, item.name) else {
            return;
        };
        self.usage.unpriced |= hosted;
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

#[cfg(test)]
mod tests;
