//! The constant-memory lexer for one Responses data line: keeps
//! `response.model`, `response.service_tier`, and the raw `response.usage`
//! object, and skips everything else, the output included.

use serde::Deserialize;

use crate::json::{Nesting, StringEnd, is_space};

/// Longest `response.usage` object kept; larger marks usage unknown.
const MAX_USAGE_BYTES: usize = 2 * 1024;
/// Longest `model` or `service_tier` kept.
const MAX_FIELD_BYTES: usize = 128;

/// `response.usage`, as OpenAI reports it.
#[derive(Deserialize)]
pub(super) struct RawUsage {
    pub(super) input_tokens: Option<u64>,
    pub(super) output_tokens: Option<u64>,
    pub(super) input_tokens_details: Option<InputDetails>,
}

#[derive(Deserialize)]
pub(super) struct InputDetails {
    pub(super) cached_tokens: Option<u64>,
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
pub(super) struct ResponseFields {
    nest: Nesting,
    started: bool,
    /// Not a JSON object, or brackets that do not match.
    pub(super) failed: bool,
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
    pub(super) fn new() -> Self {
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

    pub(super) fn feed(&mut self, chunk: &[u8]) {
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

    pub(super) fn model(&self) -> Option<&str> {
        let (bytes, len) = &self.model;
        std::str::from_utf8(bytes.get(..*len)?)
            .ok()
            .filter(|m| !m.is_empty())
    }

    pub(super) fn service_tier(&self) -> Option<&str> {
        let (bytes, len) = &self.service_tier;
        std::str::from_utf8(bytes.get(..*len)?)
            .ok()
            .filter(|t| !t.is_empty())
    }

    /// `Ok(None)` when absent or `null`; `Err` when too large or malformed.
    pub(super) fn usage(&self) -> Result<Option<RawUsage>, ()> {
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
