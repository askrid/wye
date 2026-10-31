//! The structure scanner behind [`PolicyScanner`].

use super::{MAX_MODEL_BYTES, PolicyError};
use crate::api::Api;
use crate::json::{Kept, Nesting, StringEnd, is_space, short_string_len};

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
    key: Kept<KEY_CAP>,
    /// The API's output-token fields.
    token_keys: &'static [&'static str],
    /// The string has an escape, or (a key) a non-ASCII byte: it may read
    /// as another key once decoded or case-folded.
    escaped: bool,
    /// The last top-level key was `model` and its value comes next.
    model_next: bool,
    model: Kept<MAX_MODEL_BYTES>,
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

impl PolicyScanner {
    pub fn for_api(api: Api) -> Self {
        Self {
            error: None,
            started: false,
            nest: Nesting::default(),
            in_string: false,
            string: StringEnd::default(),
            expect_key: false,
            capture: Capture::None,
            key: Kept::default(),
            token_keys: api.token_fields(),
            escaped: false,
            model_next: false,
            model: Kept::default(),
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

    /// Leaves the output-token field alone: without a ceiling, what it holds
    /// is between the client and the provider.
    pub fn without_token_field(mut self) -> Self {
        self.token_keys = &[];
        self
    }

    /// The model, once its value has been read completely.
    pub fn model(&self) -> Option<&str> {
        if !self.found {
            return None;
        }
        // Built only from complete string contents without escapes; a body
        // that is not UTF-8 there yields no model.
        std::str::from_utf8(self.model.bytes()).ok()
    }

    /// Where the model's value lies in the body, once read: its bytes
    /// exactly, since the value has no escapes. Lets a caller splice in
    /// another name and keep every other byte.
    pub fn model_span(&self) -> Option<std::ops::Range<u64>> {
        self.model()?;
        Some(self.model_start..self.model_start + self.model.bytes().len() as u64)
    }

    /// The requested output-token maximum, once its value was read completely.
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
            if let Err(error) = self.structural(chunk, byte, &mut i) {
                self.error = Some(error);
            }
        }
        self.offset += chunk.len() as u64;
    }

    /// Handles `byte`, outside strings, with `i` just after it.
    #[inline(always)]
    fn structural(&mut self, chunk: &[u8], byte: u8, i: &mut usize) -> Result<(), PolicyError> {
        if is_space(byte) {
            if self.tokens_reading {
                self.end_tokens()?;
            }
            return Ok(());
        }
        if !self.started {
            self.started = true;
            if byte != b'{' {
                return Err(PolicyError::Malformed);
            }
        }
        match byte {
            b'"' => {
                if self.tokens_next {
                    return Err(PolicyError::InvalidMaxTokens);
                }
                let top_key = self.nest.depth() == 1 && self.expect_key;
                let model_value = self.nest.depth() == 1 && self.model_next;
                self.expect_key = false;
                self.model_next = false;
                if !top_key && !model_value {
                    // Most strings: nested or ordinary values, often short.
                    let rest = chunk.get(*i..).unwrap_or_default();
                    if let Some(len) = short_string_len(rest) {
                        *i += len + 1;
                        return Ok(());
                    }
                }
                self.capture = match (top_key, model_value) {
                    (true, _) => Capture::Key,
                    (_, true) => Capture::Model,
                    _ => Capture::None,
                };
                self.key.clear();
                self.escaped = false;
                if model_value {
                    self.model.clear();
                    self.model_start = self.offset + *i as u64;
                }
                self.in_string = true;
            }
            b'{' | b'[' => {
                self.check_not_model_value()?;
                if self.tokens_next {
                    return Err(PolicyError::InvalidMaxTokens);
                }
                self.nest.open(byte == b'[');
                self.expect_key = byte == b'{';
            }
            b'}' | b']' => {
                self.end_tokens()?;
                if !self.nest.closes(byte == b']') {
                    return Err(PolicyError::Malformed);
                }
                self.check_not_model_value()?;
                self.nest.close(byte == b']');
                self.expect_key = false;
            }
            b',' => {
                self.end_tokens()?;
                self.check_not_model_value()?;
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
                self.check_not_model_value()?;
                if self.tokens_next || self.tokens_reading {
                    self.tokens_next = false;
                    self.tokens_reading = true;
                    let digit = byte.wrapping_sub(b'0');
                    self.tokens_value = (digit < 10)
                        .then(|| self.tokens_value.checked_mul(10))
                        .flatten()
                        .and_then(|v| v.checked_add(u64::from(digit)))
                        .ok_or(PolicyError::InvalidMaxTokens)?;
                }
            }
            _ => return Err(PolicyError::Malformed),
        }
        Ok(())
    }

    /// `model` takes a string; anything else where its value goes is invalid.
    #[inline(always)]
    fn check_not_model_value(&self) -> Result<(), PolicyError> {
        if self.nest.depth() == 1 && self.model_next {
            return Err(PolicyError::Invalid);
        }
        Ok(())
    }

    /// Completes a token-field value at a terminator; a key whose value
    /// never started is an error.
    fn end_tokens(&mut self) -> Result<(), PolicyError> {
        if self.tokens_reading {
            self.tokens_reading = false;
            self.max_tokens = Some(self.tokens_value);
        } else if self.tokens_next {
            return Err(PolicyError::InvalidMaxTokens);
        }
        Ok(())
    }

    fn keep(&mut self, content: &[u8]) {
        if self.capture == Capture::None {
            return;
        }
        // Go's encoding/json, for one, folds `\u212a` (the Kelvin sign) to `k`.
        if content.contains(&b'\\') || (self.capture == Capture::Key && !content.is_ascii()) {
            self.escaped = true;
        }
        match self.capture {
            Capture::Key => self.key.push(content),
            Capture::Model => self.model.push(content),
            Capture::None => {}
        }
    }

    fn end_string(&mut self) {
        match std::mem::replace(&mut self.capture, Capture::None) {
            Capture::None => {}
            // Keys compare ignoring ASCII case: some decoders (Go's) bind
            // `MODEL` to `model`.
            Capture::Key => {
                let key = self.key.whole();
                if self.escaped {
                    self.error = Some(PolicyError::EscapedKey);
                } else if key.is_some_and(|key| key.eq_ignore_ascii_case(b"model")) {
                    if self.found {
                        self.error = Some(PolicyError::Repeated);
                    }
                    self.model_next = true;
                } else if let Some(key) = key
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
                if self.escaped || self.model.is_long() {
                    self.error = Some(PolicyError::Invalid);
                } else {
                    self.found = true;
                }
            }
        }
    }
}
