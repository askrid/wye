//! The constant-memory lexer for one JSON document of a response: keeps
//! the `model`, `service_tier`, and raw `usage` members of the response
//! object (under a root key, as in a Responses SSE data line's `response`,
//! or at the top level, as in a non-streaming body), optionally counts the
//! tool calls among its content blocks or output items, and skips
//! everything else, the generated text included.

use serde::Deserialize;

use super::{MAX_TOOL_NAME_BYTES, ToolCalls, Usage};
use crate::json::{Nesting, StringEnd, is_space};

/// Longest `response.usage` object kept; larger marks usage unknown.
const MAX_USAGE_BYTES: usize = 2 * 1024;
/// Longest `model` or `service_tier` kept.
const MAX_FIELD_BYTES: usize = 128;

/// `usage`, as OpenAI's Responses API reports it.
#[derive(Deserialize)]
pub(super) struct OpenAiUsage {
    pub(super) input_tokens: Option<u64>,
    pub(super) output_tokens: Option<u64>,
    pub(super) input_tokens_details: Option<InputDetails>,
}

#[derive(Deserialize)]
pub(super) struct InputDetails {
    pub(super) cached_tokens: Option<u64>,
}

impl OpenAiUsage {
    /// Into Wye's meaning: OpenAI's `input_tokens` include cached tokens,
    /// ours do not, so the cached share becomes `cache_read_input_tokens`.
    pub(super) fn merge_into(self, usage: &mut Usage) {
        let cached = self
            .input_tokens_details
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0);
        usage.input_tokens = self.input_tokens.map(|i| i.saturating_sub(cached));
        usage.cache_read_input_tokens = self.input_tokens.map(|_| cached);
        usage.output_tokens = self.output_tokens;
    }
}

/// The response object's member keys that are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Other,
    Model,
    ServiceTier,
    Usage,
    Items,
}

/// The response's array scanned for tool calls, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Items {
    None,
    /// Messages: `content`, blocks of type `tool_use`.
    Content,
    /// Responses: `output`, items such as `function_call`.
    Output,
}

/// An item's keys that are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKey {
    Other,
    Type,
    Name,
}

/// The text kept in a fixed buffer; `None` when empty or not UTF-8.
fn slot_str<const N: usize>((bytes, len): &([u8; N], usize)) -> Option<&str> {
    bytes
        .get(..*len)
        .and_then(|b| std::str::from_utf8(b).ok())
        .filter(|t| !t.is_empty())
}

/// Hosted tools OpenAI bills per call, outside token prices.
const HOSTED_TOOLS: [&str; 4] = [
    "web_search",
    "file_search",
    "code_interpreter",
    "image_generation",
];

/// The tool call a Responses output item of type `kind` stands for: the
/// name counted, and whether it is a hosted tool billed per call. `None`
/// for messages, reasoning, and anything else that is not a call.
pub(super) fn openai_tool<'a>(kind: &'a str, name: Option<&'a str>) -> Option<(&'a str, bool)> {
    match kind {
        "function_call" | "custom_tool_call" => Some((name.unwrap_or_default(), false)),
        kind => {
            let tool = kind.strip_suffix("_call")?;
            Some((tool, HOSTED_TOOLS.contains(&tool)))
        }
    }
}

/// A constant-memory lexer for one JSON document, keeping the response
/// object's `model`, `service_tier`, and raw `usage`.
#[derive(Debug)]
pub(super) struct ResponseFields {
    /// The top-level key holding the response object; `None`: the document
    /// is the response object.
    root: Option<&'static [u8]>,
    nest: Nesting,
    started: bool,
    /// Not a JSON object, or brackets that do not match.
    pub(super) failed: bool,
    in_string: bool,
    string: StringEnd,
    expect_key: bool,
    /// The depth-1 key being read is the root key.
    in_root: bool,
    /// The response object's key whose value comes next.
    field: Field,
    /// The current string is kept: a key that can matter, or a field's value.
    capturing: bool,
    is_key: bool,
    text: [u8; MAX_FIELD_BYTES],
    text_len: usize,
    text_long: bool,
    model: ([u8; MAX_FIELD_BYTES], usize),
    service_tier: ([u8; MAX_FIELD_BYTES], usize),
    /// The usage object's bytes; empty when absent or `null`.
    /// Inline, so reading a response allocates nothing beyond the scanner.
    usage: [u8; MAX_USAGE_BYTES],
    usage_len: usize,
    /// Inside the usage object.
    in_usage: bool,
    usage_long: bool,
    items: Items,
    /// Inside the items array.
    in_items: bool,
    item_key: ItemKey,
    item_type: ([u8; MAX_TOOL_NAME_BYTES], usize),
    item_name: ([u8; MAX_TOOL_NAME_BYTES], usize),
    /// Tool calls among the items; boxed on the first.
    pub(super) tools: Option<Box<ToolCalls>>,
    /// An item was a hosted tool billed per call.
    pub(super) hosted_tool: bool,
}

impl ResponseFields {
    pub(super) fn new(root: Option<&'static [u8]>, items: Items) -> Self {
        Self {
            root,
            nest: Nesting::default(),
            started: false,
            failed: false,
            in_string: false,
            string: StringEnd::default(),
            expect_key: false,
            in_root: false,
            field: Field::Other,
            capturing: false,
            is_key: false,
            text: [0; MAX_FIELD_BYTES],
            text_len: 0,
            text_long: false,
            model: ([0; MAX_FIELD_BYTES], 0),
            service_tier: ([0; MAX_FIELD_BYTES], 0),
            usage: [0; MAX_USAGE_BYTES],
            usage_len: 0,
            in_usage: false,
            usage_long: false,
            items,
            in_items: false,
            item_key: ItemKey::Other,
            item_type: ([0; MAX_TOOL_NAME_BYTES], 0),
            item_name: ([0; MAX_TOOL_NAME_BYTES], 0),
            tools: None,
            hosted_tool: false,
        }
    }

    /// Depth of the response object's members: 2 under a root key, else 1.
    fn field_depth(&self) -> u32 {
        if self.root.is_some() { 2 } else { 1 }
    }

    /// Inside the response object.
    fn in_object(&self) -> bool {
        self.root.is_none() || self.in_root
    }

    pub(super) fn feed(&mut self, chunk: &[u8]) {
        let fields = self.field_depth();
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
                    let in_item = self.in_items && depth == fields + 2;
                    self.capturing = if self.is_key {
                        (depth == 1 && self.root.is_some())
                            || (depth == fields && self.in_object())
                            || in_item
                    } else {
                        (depth == fields && matches!(self.field, Field::Model | Field::ServiceTier))
                            || (in_item && self.item_key != ItemKey::Other)
                    };
                    self.text_len = 0;
                    self.text_long = false;
                    self.in_string = true;
                    self.string.start();
                }
                b'{' | b'[' => {
                    let depth = self.nest.depth();
                    if depth == fields && self.field == Field::Usage && byte == b'{' {
                        self.in_usage = true;
                        usage_from = Some(i);
                    }
                    if depth == fields && self.field == Field::Items && byte == b'[' {
                        self.in_items = true;
                    }
                    if self.in_items && depth == fields + 1 && byte == b'{' {
                        self.item_key = ItemKey::Other;
                        self.item_type.1 = 0;
                        self.item_name.1 = 0;
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
                    if self.in_usage && depth == fields {
                        self.in_usage = false;
                        if let Some(from) = usage_from.take() {
                            self.keep_usage(chunk.get(from..=i).unwrap_or_default());
                        }
                    }
                    if self.in_items && depth == fields + 1 && byte == b'}' {
                        self.end_item();
                    }
                    if self.in_items && depth == fields {
                        self.in_items = false;
                    }
                    if depth < fields {
                        self.field = Field::Other;
                    }
                    self.expect_key = false;
                }
                b',' => {
                    let depth = self.nest.depth();
                    self.expect_key = depth > 0 && !self.nest.is_array(depth);
                    if depth <= fields {
                        self.field = Field::Other;
                    }
                    if depth == fields + 2 {
                        self.item_key = ItemKey::Other;
                    }
                    if depth == 1 {
                        self.in_root = false;
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
        match self
            .usage
            .get_mut(self.usage_len..self.usage_len + bytes.len())
        {
            Some(slot) => {
                slot.copy_from_slice(bytes);
                self.usage_len += bytes.len();
            }
            None => self.usage_long = true,
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
            let fields = self.field_depth();
            if let Some(root) = self.root.filter(|_| depth == 1) {
                self.in_root = text == root;
            } else if depth == fields {
                let items = match self.items {
                    Items::None => None,
                    Items::Content => Some(&b"content"[..]),
                    Items::Output => Some(&b"output"[..]),
                };
                self.field = match text {
                    b"model" => Field::Model,
                    b"service_tier" => Field::ServiceTier,
                    b"usage" => Field::Usage,
                    _ if items == Some(text) => Field::Items,
                    _ => Field::Other,
                };
            } else {
                self.item_key = match text {
                    b"type" => ItemKey::Type,
                    b"name" => ItemKey::Name,
                    _ => ItemKey::Other,
                };
            }
            return;
        }
        if depth != self.field_depth() {
            let slot = match self.item_key {
                ItemKey::Type => &mut self.item_type,
                ItemKey::Name => &mut self.item_name,
                ItemKey::Other => return,
            };
            if let Some(dst) = slot.0.get_mut(..text.len()) {
                dst.copy_from_slice(text);
                slot.1 = text.len();
            }
            return;
        }
        let slot = match self.field {
            Field::Model => &mut self.model,
            Field::ServiceTier => &mut self.service_tier,
            Field::Other | Field::Usage | Field::Items => return,
        };
        if let Some(dst) = slot.0.get_mut(..text.len()) {
            dst.copy_from_slice(text);
            slot.1 = text.len();
        }
    }

    /// An item ended: count it if it is a tool call.
    fn end_item(&mut self) {
        let (Some(kind), name) = (slot_str(&self.item_type), slot_str(&self.item_name)) else {
            return;
        };
        let call = match self.items {
            Items::Content => (kind == "tool_use").then(|| (name.unwrap_or_default(), false)),
            Items::Output => openai_tool(kind, name),
            Items::None => None,
        };
        if let Some((tool, hosted)) = call {
            self.hosted_tool |= hosted;
            self.tools
                .get_or_insert_with(|| Box::new(ToolCalls::new()))
                .count(tool);
        }
    }

    pub(super) fn model(&self) -> Option<&str> {
        slot_str(&self.model)
    }

    pub(super) fn service_tier(&self) -> Option<&str> {
        slot_str(&self.service_tier)
    }

    /// The raw usage object: `Ok(None)` when absent or `null`; `Err` when
    /// too large or cut off.
    pub(super) fn usage_bytes(&self) -> Result<Option<&[u8]>, ()> {
        if self.usage_long || self.in_usage {
            return Err(());
        }
        Ok(self.usage.get(..self.usage_len).filter(|u| !u.is_empty()))
    }

    /// The usage object in OpenAI's shape; `Err` when too large or malformed.
    pub(super) fn openai_usage(&self) -> Result<Option<OpenAiUsage>, ()> {
        match self.usage_bytes()? {
            None => Ok(None),
            Some(bytes) => serde_json::from_slice(bytes).map(Some).map_err(|_| ()),
        }
    }
}
