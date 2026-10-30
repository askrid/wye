//! The structure scanner behind [`UpgradeScanner`].

use super::{Findings, Thinking};
use crate::json::{Nesting, StringEnd, is_space, short_string_len};

/// Member keys the rules name, tracked for the first three levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Key {
    #[default]
    Other,
    Thinking,
    ToolChoice,
    Tools,
    Messages,
    Type,
    Role,
}

/// Levels whose keys are tracked: the request object (1), a top-level
/// member's object or array (2), and an array element's object (3).
const KEY_DEPTHS: usize = 4;
/// Bytes kept of a key or a watched value; longer ones match nothing.
const CAPTURE: usize = 32;

/// Incremental reader of the request fields upgrade rules name, with constant
/// memory and chunk boundaries anywhere.
pub struct UpgradeScanner {
    findings: Findings,
    nest: Nesting,
    started: bool,
    /// The body is not a JSON object or its brackets do not match: stop.
    failed: bool,
    in_string: bool,
    string: StringEnd,
    /// Inside an object, the next string is a key.
    expect_key: bool,
    /// The key of the member being read, by depth.
    keys: [Key; KEY_DEPTHS],
    /// The current string is kept: a tracked key or a watched value.
    capturing: bool,
    is_key: bool,
    buf: [u8; CAPTURE],
    len: usize,
    long: bool,
    escaped: bool,
}

impl Default for UpgradeScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl UpgradeScanner {
    pub fn new() -> Self {
        Self {
            findings: Findings::default(),
            nest: Nesting::default(),
            started: false,
            failed: false,
            in_string: false,
            string: StringEnd::default(),
            expect_key: false,
            keys: [Key::Other; KEY_DEPTHS],
            capturing: false,
            is_key: false,
            buf: [0; CAPTURE],
            len: 0,
            long: false,
            escaped: false,
        }
    }

    /// What the body uses so far; complete once the body has ended.
    pub fn findings(&self) -> Findings {
        self.findings
    }

    /// Feeds the next chunk of request bytes.
    pub fn feed(&mut self, chunk: &[u8]) {
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
                        self.after_string();
                        i += quote + 1;
                    }
                    None => i = chunk.len(),
                }
                continue;
            }
            let Some(&byte) = chunk.get(i) else { break };
            i += 1;
            if is_space(byte) {
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
                    self.capturing = self.watched();
                    if !self.capturing {
                        // Most strings: unwatched, often short.
                        let rest = chunk.get(i..).unwrap_or_default();
                        if let Some(len) = short_string_len(rest) {
                            i += len + 1;
                            self.after_string();
                            continue;
                        }
                    }
                    self.len = 0;
                    self.long = false;
                    self.escaped = false;
                    self.in_string = true;
                    self.string.start();
                }
                b'{' | b'[' => {
                    self.nest.open(byte == b'[');
                    if let Some(key) = self.keys.get_mut(self.nest.depth() as usize) {
                        *key = Key::Other;
                    }
                    self.expect_key = byte == b'{';
                }
                b'}' | b']' => {
                    if !self.nest.close(byte == b']') {
                        self.failed = true;
                        break;
                    }
                    self.expect_key = false;
                }
                b',' => self.expect_key = self.nest.in_object(),
                b':' => self.expect_key = false,
                _ => {}
            }
        }
    }

    /// Whether the string starting now is kept.
    fn watched(&self) -> bool {
        let depth = self.nest.depth() as usize;
        if self.is_key {
            return (1..KEY_DEPTHS).contains(&depth);
        }
        let key = |d: usize| self.keys.get(d).copied().unwrap_or_default();
        match depth {
            2 => matches!(key(1), Key::Thinking | Key::ToolChoice) && key(2) == Key::Type,
            3 => {
                self.nest.is_array(2)
                    && matches!(
                        (key(1), key(3)),
                        (Key::Tools, Key::Type) | (Key::Messages, Key::Role)
                    )
            }
            _ => false,
        }
    }

    fn keep(&mut self, content: &[u8]) {
        if content.contains(&b'\\') {
            self.escaped = true;
        }
        match self.buf.get_mut(self.len..self.len + content.len()) {
            Some(slot) => {
                slot.copy_from_slice(content);
                self.len += content.len();
            }
            None => self.long = true,
        }
    }

    /// After a key, its value comes; after a value, a separator.
    fn after_string(&mut self) {
        self.expect_key = false;
    }

    fn end_string(&mut self) {
        // An escaped or long string matches nothing the rules name.
        let text: &[u8] = if self.escaped || self.long {
            b""
        } else {
            self.buf.get(..self.len).unwrap_or_default()
        };
        let depth = self.nest.depth() as usize;
        if self.is_key {
            let key = match (depth, text) {
                (1, b"thinking") => Key::Thinking,
                (1, b"tool_choice") => Key::ToolChoice,
                (1, b"tools") => Key::Tools,
                (1, b"messages") => Key::Messages,
                (1, b"temperature" | b"top_p" | b"top_k") => {
                    self.findings.sampling = true;
                    Key::Other
                }
                (2 | 3, b"type") => Key::Type,
                (2 | 3, b"role") => Key::Role,
                _ => Key::Other,
            };
            if let Some(slot) = self.keys.get_mut(depth) {
                *slot = key;
            }
            return;
        }
        let parent = self.keys.get(1).copied().unwrap_or_default();
        let f = &mut self.findings;
        match (depth, parent) {
            (2, Key::Thinking) => {
                f.thinking = Some(match text {
                    b"disabled" => Thinking::Disabled,
                    b"enabled" => Thinking::Enabled,
                    _ => Thinking::Other,
                });
            }
            (2, Key::ToolChoice) => f.forced_tool_choice = matches!(text, b"any" | b"tool"),
            (3, Key::Tools) => {
                f.computer_tool |= matches!(text, b"computer_20250124" | b"computer_20251124");
            }
            (3, Key::Messages) => {
                f.prefill = text == b"assistant";
                f.system_role |= text == b"system";
            }
            _ => {}
        }
    }
}
