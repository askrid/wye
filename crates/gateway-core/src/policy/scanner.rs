//! The structure scanner behind [`PolicyScanner`].

use super::{MAX_MODEL_BYTES, PolicyError};
use crate::api::Api;
use crate::json::{Nesting, StringEnd, is_space, short_string_len};

/// What the bytes of the current top-level string are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    None,
    /// A top-level key, kept to compare with `model`.
    Key,
    /// The value of `model`.
    Model,
}

/// Bytes kept of a top-level key: enough to compare with the longest
/// output-token field, `max_completion_tokens`.
const KEY_CAP: usize = 24;

/// Incremental reader of one request body's top-level `model` and
/// output-token field.
pub struct PolicyScanner {
    error: Option<PolicyError>,
    started: bool,
    nest: Nesting,
    in_string: bool,
    string: StringEnd,
    expect_key: bool,
    capture: Capture,
    key: [u8; KEY_CAP],
    /// The API's output-token fields (`max_tokens`, `max_output_tokens`,
    /// `max_completion_tokens`).
    token_keys: &'static [&'static str],
    key_len: usize,
    key_long: bool,
    /// The string has an escape, or (a key) a non-ASCII byte: it may read
    /// as another key once decoded or case-folded.
    escaped: bool,
    /// The last top-level key was `model` and its value comes next.
    model_next: bool,
    model: [u8; MAX_MODEL_BYTES],
    model_len: usize,
    /// Offset of the model value's first byte in the body.
    model_start: u64,
    found: bool,
    /// Body bytes fed before the current chunk.
    offset: u64,
    /// The last top-level key was the token field and its value comes next.
    tokens_next: bool,
    /// Digits of the token field are being read.
    tokens_reading: bool,
    tokens_value: u64,
    tokens_seen: bool,
    /// The token field, once its value has been read completely.
    max_tokens: Option<u64>,
}

impl Default for PolicyScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyScanner {
    /// A scanner for Anthropic Messages bodies.
    pub fn new() -> Self {
        Self::for_api(Api::Messages)
    }

    /// A scanner for `api`'s request bodies.
    pub fn for_api(api: Api) -> Self {
        Self {
            error: None,
            started: false,
            nest: Nesting::default(),
            in_string: false,
            string: StringEnd::default(),
            expect_key: false,
            capture: Capture::None,
            key: [0; KEY_CAP],
            token_keys: api.token_fields(),
            key_len: 0,
            key_long: false,
            escaped: false,
            model_next: false,
            model: [0; MAX_MODEL_BYTES],
            model_len: 0,
            model_start: 0,
            found: false,
            offset: 0,
            tokens_next: false,
            tokens_reading: false,
            tokens_value: 0,
            tokens_seen: false,
            max_tokens: None,
        }
    }

    /// The model, once its value has been read completely.
    pub fn model(&self) -> Option<&str> {
        if !self.found {
            return None;
        }
        // Built only from complete string contents without escapes; a body
        // that is not UTF-8 there yields no model.
        std::str::from_utf8(self.model.get(..self.model_len)?).ok()
    }

    /// Where the model's value lies in the body, once read: its bytes
    /// exactly, since the value has no escapes. Lets a caller splice in
    /// another name and keep every other byte.
    pub fn model_span(&self) -> Option<std::ops::Range<u64>> {
        self.model()?;
        Some(self.model_start..self.model_start + self.model_len as u64)
    }

    /// The requested output-token maximum (`max_tokens` on Messages,
    /// `max_output_tokens` on Responses, either field on Chat Completions),
    /// once its value was read completely.
    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    /// The first problem found, if any. Scanning stops there.
    pub fn error(&self) -> Option<PolicyError> {
        self.error
    }

    /// The outcome once the body has ended.
    pub fn finish(&self) -> Result<&str, PolicyError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        self.model().ok_or(if self.found {
            PolicyError::Invalid
        } else {
            PolicyError::Missing
        })
    }

    /// Feeds the next chunk of request bytes.
    pub fn feed(&mut self, chunk: &[u8]) {
        let mut i = 0;
        while i < chunk.len() && self.error.is_none() {
            if self.in_string {
                let rest = chunk.get(i..).unwrap_or_default();
                let end = self.string.find(rest);
                let content = rest.get(..end.unwrap_or(rest.len())).unwrap_or_default();
                self.keep(content);
                match end {
                    Some(quote) => {
                        self.in_string = false;
                        self.end_string();
                        i += quote + 1;
                    }
                    None => i = chunk.len(),
                }
                continue;
            }
            let Some(&byte) = chunk.get(i) else { break };
            i += 1;
            if is_space(byte) {
                if self.tokens_reading {
                    self.end_tokens();
                }
                continue;
            }
            if !self.started {
                self.started = true;
                if byte != b'{' {
                    self.error = Some(PolicyError::Malformed);
                    break;
                }
            }
            match byte {
                b'"' => {
                    if self.tokens_next {
                        self.error = Some(PolicyError::InvalidMaxTokens);
                        break;
                    }
                    let top_key = self.nest.depth() == 1 && self.expect_key;
                    let model_value = self.nest.depth() == 1 && self.model_next;
                    self.expect_key = false;
                    self.model_next = false;
                    if !top_key && !model_value {
                        // Most strings: nested or ordinary values, often short.
                        let rest = chunk.get(i..).unwrap_or_default();
                        if let Some(len) = short_string_len(rest) {
                            i += len + 1;
                            continue;
                        }
                    }
                    self.capture = match (top_key, model_value) {
                        (true, _) => Capture::Key,
                        (_, true) => Capture::Model,
                        _ => Capture::None,
                    };
                    self.key_len = 0;
                    self.key_long = false;
                    self.escaped = false;
                    if model_value {
                        self.model_len = 0;
                        self.model_start = self.offset + i as u64;
                    }
                    self.in_string = true;
                    self.string.start();
                }
                b'{' | b'[' => {
                    if self.nest.depth() == 1 && self.model_next {
                        self.error = Some(PolicyError::Invalid);
                        break;
                    }
                    if self.tokens_next {
                        self.error = Some(PolicyError::InvalidMaxTokens);
                        break;
                    }
                    self.nest.open(byte == b'[');
                    self.expect_key = byte == b'{';
                }
                b'}' | b']' => {
                    self.end_tokens();
                    if self.error.is_some() {
                        break;
                    }
                    if !self.nest.closes(byte == b']') {
                        self.error = Some(PolicyError::Malformed);
                        break;
                    }
                    if self.nest.depth() == 1 && self.model_next {
                        self.error = Some(PolicyError::Invalid);
                        break;
                    }
                    self.nest.close(byte == b']');
                    self.expect_key = false;
                }
                b',' => {
                    self.end_tokens();
                    if self.error.is_some() {
                        break;
                    }
                    if self.nest.depth() == 1 && self.model_next {
                        self.error = Some(PolicyError::Invalid);
                        break;
                    }
                    self.expect_key = self.nest.in_object();
                }
                b':' => {}
                // A number, `true`, `false`, or `null`. Any other byte is not
                // JSON, and a provider that reads it anyway (a NUL in UTF-16
                // text, an unquoted key, a comment) could see other keys than
                // these.
                b'0'..=b'9'
                | b'-'
                | b'+'
                | b'.'
                | b'E'
                | b'a'
                | b'e'
                | b'f'
                | b'l'
                | b'n'
                | b'r'
                | b's'
                | b't'
                | b'u' => {
                    if self.nest.depth() == 1 && self.model_next {
                        self.error = Some(PolicyError::Invalid);
                        break;
                    }
                    if self.tokens_next || self.tokens_reading {
                        self.tokens_next = false;
                        self.tokens_reading = true;
                        let digit = byte.wrapping_sub(b'0');
                        let next = (digit < 10)
                            .then(|| self.tokens_value.checked_mul(10))
                            .flatten()
                            .and_then(|v| v.checked_add(u64::from(digit)));
                        match next {
                            Some(value) => self.tokens_value = value,
                            None => {
                                self.error = Some(PolicyError::InvalidMaxTokens);
                                break;
                            }
                        }
                    }
                }
                _ => {
                    self.error = Some(PolicyError::Malformed);
                    break;
                }
            }
        }
        self.offset += chunk.len() as u64;
    }

    /// Completes a token-field value at a terminator; a key whose value
    /// never started is an error.
    fn end_tokens(&mut self) {
        if self.tokens_reading {
            self.tokens_reading = false;
            self.max_tokens = Some(self.tokens_value);
        } else if self.tokens_next {
            self.error = Some(PolicyError::InvalidMaxTokens);
        }
    }

    /// Keeps string content that matters.
    fn keep(&mut self, content: &[u8]) {
        if self.capture == Capture::None {
            return;
        }
        // Go's encoding/json, for one, folds `\u212a` (the Kelvin sign) to `k`.
        if content.contains(&b'\\') || (self.capture == Capture::Key && !content.is_ascii()) {
            self.escaped = true;
        }
        let (buf, len, long) = match self.capture {
            Capture::Key => (&mut self.key[..], &mut self.key_len, &mut self.key_long),
            Capture::Model => (&mut self.model[..], &mut self.model_len, &mut self.key_long),
            Capture::None => return,
        };
        match buf.get_mut(*len..*len + content.len()) {
            Some(slot) => {
                slot.copy_from_slice(content);
                *len += content.len();
            }
            None => *long = true,
        }
    }

    fn end_string(&mut self) {
        match std::mem::replace(&mut self.capture, Capture::None) {
            Capture::None => {}
            // Keys compare ignoring ASCII case: some decoders (Go's) bind
            // `MODEL` to `model`.
            Capture::Key => {
                if self.escaped {
                    self.error = Some(PolicyError::EscapedKey);
                } else if !self.key_long
                    && self
                        .key
                        .get(..self.key_len)
                        .is_some_and(|key| key.eq_ignore_ascii_case(b"model"))
                {
                    if self.found {
                        self.error = Some(PolicyError::Repeated);
                    }
                    self.model_next = true;
                } else if !self.key_long
                    && let Some(key) = self.key.get(..self.key_len)
                    && self
                        .token_keys
                        .iter()
                        .any(|k| k.as_bytes().eq_ignore_ascii_case(key))
                {
                    if self.tokens_seen {
                        self.error = Some(PolicyError::RepeatedMaxTokens);
                    }
                    self.tokens_seen = true;
                    self.tokens_next = true;
                }
            }
            Capture::Model => {
                if self.escaped || self.key_long {
                    self.error = Some(PolicyError::Invalid);
                } else {
                    self.found = true;
                }
            }
        }
    }
}
