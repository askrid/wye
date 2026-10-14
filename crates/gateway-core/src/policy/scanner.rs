//! The structure scanner behind [`PolicyScanner`].

use super::{MAX_MODEL_BYTES, PolicyError};
use crate::json::{StringEnd, is_space, short_string_len};

/// What the bytes of the current top-level string are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    None,
    /// A top-level key, kept to compare with `model`.
    Key,
    /// The value of `model`.
    Model,
}

/// Bytes kept of a top-level key: enough to compare with `max_tokens`.
const KEY_CAP: usize = 16;

/// Incremental reader of one request body's top-level `model`.
pub struct PolicyScanner {
    error: Option<PolicyError>,
    started: bool,
    depth: u32,
    /// Bit `d` is set when the container at depth `d` is an array (`d < 64`).
    arrays: u64,
    in_string: bool,
    string: StringEnd,
    expect_key: bool,
    capture: Capture,
    key: [u8; KEY_CAP],
    key_len: usize,
    key_long: bool,
    escaped: bool,
    /// The last top-level key was `model` and its value comes next.
    model_next: bool,
    model: [u8; MAX_MODEL_BYTES],
    model_len: usize,
    found: bool,
    /// The last top-level key was `max_tokens` and its value comes next.
    tokens_next: bool,
    /// Digits of `max_tokens` are being read.
    tokens_reading: bool,
    tokens_value: u64,
    tokens_seen: bool,
    /// `max_tokens`, once its value has been read completely.
    max_tokens: Option<u64>,
}

impl Default for PolicyScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyScanner {
    pub fn new() -> Self {
        Self {
            error: None,
            started: false,
            depth: 0,
            arrays: 0,
            in_string: false,
            string: StringEnd::default(),
            expect_key: false,
            capture: Capture::None,
            key: [0; KEY_CAP],
            key_len: 0,
            key_long: false,
            escaped: false,
            model_next: false,
            model: [0; MAX_MODEL_BYTES],
            model_len: 0,
            found: false,
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

    /// `max_tokens`, once its value has been read completely.
    pub fn max_tokens(&self) -> Option<u64> {
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
                    let top_key = self.depth == 1 && self.expect_key;
                    let model_value = self.depth == 1 && self.model_next;
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
                    }
                    self.in_string = true;
                    self.string.start();
                }
                b'{' | b'[' => {
                    if self.depth == 1 && self.model_next {
                        self.error = Some(PolicyError::Invalid);
                        break;
                    }
                    if self.tokens_next {
                        self.error = Some(PolicyError::InvalidMaxTokens);
                        break;
                    }
                    self.depth = self.depth.saturating_add(1);
                    if self.depth < 64 {
                        if byte == b'[' {
                            self.arrays |= 1 << self.depth;
                        } else {
                            self.arrays &= !(1 << self.depth);
                        }
                    }
                    self.expect_key = byte == b'{';
                }
                b'}' | b']' => {
                    self.end_tokens();
                    if self.error.is_some() {
                        break;
                    }
                    let is_array = self.depth < 64 && self.arrays & (1 << self.depth) != 0;
                    if self.depth == 0 || (self.depth < 64 && is_array != (byte == b']')) {
                        self.error = Some(PolicyError::Malformed);
                        break;
                    }
                    if self.depth == 1 && self.model_next {
                        self.error = Some(PolicyError::Invalid);
                        break;
                    }
                    self.depth -= 1;
                    self.expect_key = false;
                }
                b',' => {
                    self.end_tokens();
                    if self.error.is_some() {
                        break;
                    }
                    if self.depth == 1 && self.model_next {
                        self.error = Some(PolicyError::Invalid);
                        break;
                    }
                    let in_object = self.depth > 0
                        && !(self.depth < 64 && self.arrays & (1 << self.depth) != 0);
                    self.expect_key = in_object;
                }
                b':' => {}
                // A number, `true`, `false`, or `null`.
                _ => {
                    if self.depth == 1 && self.model_next {
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
            }
        }
    }

    /// Completes a `max_tokens` value at a terminator; a key whose value
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
        if content.contains(&b'\\') {
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
            Capture::Key => {
                if self.escaped {
                    self.error = Some(PolicyError::EscapedKey);
                } else if !self.key_long && self.key.get(..self.key_len) == Some(b"model") {
                    if self.found {
                        self.error = Some(PolicyError::Repeated);
                    }
                    self.model_next = true;
                } else if !self.key_long && self.key.get(..self.key_len) == Some(b"max_tokens") {
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
