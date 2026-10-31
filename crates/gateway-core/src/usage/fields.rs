//! The constant-memory lexer for one JSON document of a response: keeps
//! the `model`, `service_tier`, and raw `usage` members of the response
//! object (under a root key, as in a Responses SSE data line's `response`,
//! or at the top level, as in a non-streaming body), optionally counts the
//! tool calls among its content blocks or output items, and skips
//! everything else, the generated text included.

use serde::Deserialize;

use super::{MAX_TOOL_NAME_BYTES, ToolCalls, Usage, count_tool};
use crate::json::{Kept, Lexeme, Lexer};

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
    pub(super) fn merge_into(self, usage: &mut Usage) {
        merge_openai(
            usage,
            self.input_tokens,
            self.output_tokens,
            self.input_tokens_details,
        );
    }
}

/// Into Wye's meaning: OpenAI's input count includes cached tokens, ours
/// does not, so the cached share becomes `cache_read_input_tokens`.
pub(super) fn merge_openai(
    usage: &mut Usage,
    input: Option<u64>,
    output: Option<u64>,
    details: Option<InputDetails>,
) {
    let cached = details.and_then(|d| d.cached_tokens).unwrap_or(0);
    usage.input_tokens = input.map(|i| i.saturating_sub(cached));
    usage.cache_read_input_tokens = input.map(|_| cached);
    usage.output_tokens = output;
}

/// The response object's member keys that are read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Field {
    #[default]
    Other,
    Model,
    ServiceTier,
    Usage,
    Items,
}

/// The response's array scanned for tool calls, if any.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Items {
    #[default]
    None,
    /// Messages: `content`, blocks of type `tool_use`.
    Content,
    /// Responses: `output`, items such as `function_call`.
    Output,
}

/// An item's keys that are read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ItemKey {
    #[default]
    Other,
    Type,
    Name,
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

#[derive(Debug, Default)]
pub(super) struct ResponseFields {
    /// The top-level key holding the response object; `None`: the document
    /// is the response object.
    root: Option<&'static [u8]>,
    lex: Lexer<MAX_FIELD_BYTES>,
    /// The depth-1 key being read is the root key.
    in_root: bool,
    /// The response object's key whose value comes next.
    field: Field,
    model: Kept<MAX_FIELD_BYTES>,
    service_tier: Kept<MAX_FIELD_BYTES>,
    /// The usage object's bytes; empty when absent or `null`.
    /// Inline, so reading a response allocates nothing beyond the scanner.
    usage: Kept<MAX_USAGE_BYTES>,
    in_usage: bool,
    items: Items,
    in_items: bool,
    item_key: ItemKey,
    item_type: Kept<MAX_TOOL_NAME_BYTES>,
    item_name: Kept<MAX_TOOL_NAME_BYTES>,
    /// Tool calls among the items; boxed on the first.
    pub(super) tools: Option<Box<ToolCalls>>,
    /// An item was a hosted tool billed per call.
    pub(super) hosted_tool: bool,
}

impl ResponseFields {
    pub(super) fn new(root: Option<&'static [u8]>, items: Items) -> Self {
        Self {
            root,
            items,
            ..Self::default()
        }
    }

    pub(super) fn failed(&self) -> bool {
        self.lex.failed
    }

    /// Depth of the response object's members: 2 under a root key, else 1.
    fn field_depth(&self) -> u32 {
        if self.root.is_some() { 2 } else { 1 }
    }

    fn in_object(&self) -> bool {
        self.root.is_none() || self.in_root
    }

    pub(super) fn feed(&mut self, chunk: &[u8]) {
        let fields = self.field_depth();
        // Where the usage bytes of this chunk start, while inside usage.
        let mut usage_from = self.in_usage.then_some(0);
        let mut i = 0;
        while let Some(lexeme) = self.lex.next(chunk, &mut i) {
            let byte = match lexeme {
                Lexeme::Kept => {
                    self.end_string();
                    continue;
                }
                Lexeme::Byte(byte) => byte,
            };
            match byte {
                b'"' => {
                    self.lex.is_key = self.lex.expect_key;
                    let depth = self.lex.nest.depth();
                    let in_item = self.in_items && depth == fields + 2;
                    let capturing = if self.lex.is_key {
                        (depth == 1 && self.root.is_some())
                            || (depth == fields && self.in_object())
                            || in_item
                    } else {
                        (depth == fields && matches!(self.field, Field::Model | Field::ServiceTier))
                            || (in_item && self.item_key != ItemKey::Other)
                    };
                    self.lex.start_string(capturing);
                }
                b'{' | b'[' => {
                    let depth = self.lex.nest.depth();
                    if depth == fields && self.field == Field::Usage && byte == b'{' {
                        self.in_usage = true;
                        usage_from = Some(i - 1);
                    }
                    if depth == fields && self.field == Field::Items && byte == b'[' {
                        self.in_items = true;
                    }
                    if self.in_items && depth == fields + 1 && byte == b'{' {
                        self.item_key = ItemKey::Other;
                        self.item_type.clear();
                        self.item_name.clear();
                    }
                    self.lex.open(byte);
                }
                b'}' | b']' => {
                    self.lex.close(byte);
                    if self.lex.failed {
                        break;
                    }
                    let depth = self.lex.nest.depth();
                    if self.in_usage && depth == fields {
                        self.in_usage = false;
                        if let Some(from) = usage_from.take() {
                            self.usage.push(chunk.get(from..i).unwrap_or_default());
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
                }
                b',' => {
                    let depth = self.lex.nest.depth();
                    self.lex.expect_key = self.lex.nest.in_object();
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
                b':' => self.lex.expect_key = false,
                _ => {}
            }
        }
        if self.in_usage
            && let Some(from) = usage_from
        {
            self.usage.push(chunk.get(from..).unwrap_or_default());
        }
    }

    fn end_string(&mut self) {
        let text = self.lex.text.whole().unwrap_or_default();
        let depth = self.lex.nest.depth();
        if self.lex.is_key {
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
            match self.item_key {
                ItemKey::Type => self.item_type.set(text),
                ItemKey::Name => self.item_name.set(text),
                ItemKey::Other => {}
            }
            return;
        }
        match self.field {
            Field::Model => self.model.set(text),
            Field::ServiceTier => self.service_tier.set(text),
            Field::Other | Field::Usage | Field::Items => {}
        }
    }

    /// An item ended: count it if it is a tool call.
    fn end_item(&mut self) {
        let (Some(kind), name) = (self.item_type.text(), self.item_name.text()) else {
            return;
        };
        let call = match self.items {
            Items::Content => (kind == "tool_use").then(|| (name.unwrap_or_default(), false)),
            Items::Output => openai_tool(kind, name),
            Items::None => None,
        };
        if let Some((tool, hosted)) = call {
            self.hosted_tool |= hosted;
            count_tool(&mut self.tools, tool);
        }
    }

    pub(super) fn model(&self) -> Option<&str> {
        self.model.text()
    }

    pub(super) fn service_tier(&self) -> Option<&str> {
        self.service_tier.text()
    }

    /// The raw usage object: `Ok(None)` when absent or `null`; `Err` when
    /// too large or cut off.
    pub(super) fn usage_bytes(&self) -> Result<Option<&[u8]>, ()> {
        match self.usage.whole() {
            Some(usage) if !self.in_usage => Ok(Some(usage).filter(|u| !u.is_empty())),
            _ => Err(()),
        }
    }

    /// The usage object in OpenAI's shape; `Err` when too large or malformed.
    pub(super) fn openai_usage(&self) -> Result<Option<OpenAiUsage>, ()> {
        match self.usage_bytes()? {
            None => Ok(None),
            Some(bytes) => serde_json::from_slice(bytes).map(Some).map_err(|_| ()),
        }
    }
}
