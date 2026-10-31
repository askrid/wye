//! The structure scanner behind [`UpgradeScanner`].

use super::{Findings, Thinking};
use crate::json::{Lexeme, Lexer, short_string_len};

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
#[derive(Default)]
pub struct UpgradeScanner {
    findings: Findings,
    lex: Lexer<CAPTURE>,
    /// The key of the member being read, by depth.
    keys: [Key; KEY_DEPTHS],
}

impl UpgradeScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the body uses so far; complete once the body has ended.
    pub fn findings(&self) -> Findings {
        self.findings
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        let mut i = 0;
        while let Some(lexeme) = self.lex.next(chunk, &mut i) {
            match lexeme {
                Lexeme::Kept => self.end_string(),
                Lexeme::Byte(b'"') => {
                    self.lex.is_key = self.lex.expect_key;
                    let capturing = self.watched();
                    if !capturing {
                        // Most strings: unwatched, often short.
                        let rest = chunk.get(i..).unwrap_or_default();
                        if let Some(len) = short_string_len(rest) {
                            i += len + 1;
                            self.lex.expect_key = false;
                            continue;
                        }
                    }
                    self.lex.start_string(capturing);
                }
                Lexeme::Byte(byte @ (b'{' | b'[')) => {
                    self.lex.open(byte);
                    if let Some(key) = self.keys.get_mut(self.lex.nest.depth() as usize) {
                        *key = Key::Other;
                    }
                }
                Lexeme::Byte(byte @ (b'}' | b']')) => self.lex.close(byte),
                Lexeme::Byte(b',') => self.lex.expect_key = self.lex.nest.in_object(),
                Lexeme::Byte(b':') => self.lex.expect_key = false,
                Lexeme::Byte(_) => {}
            }
        }
    }

    /// Whether the string starting now is kept.
    fn watched(&self) -> bool {
        let depth = self.lex.nest.depth() as usize;
        if self.lex.is_key {
            return (1..KEY_DEPTHS).contains(&depth);
        }
        let key = |d: usize| self.keys.get(d).copied().unwrap_or_default();
        match depth {
            2 => matches!(key(1), Key::Thinking | Key::ToolChoice) && key(2) == Key::Type,
            3 => {
                self.lex.nest.is_array(2)
                    && matches!(
                        (key(1), key(3)),
                        (Key::Tools, Key::Type) | (Key::Messages, Key::Role)
                    )
            }
            _ => false,
        }
    }

    fn end_string(&mut self) {
        // An escaped or long string matches nothing the rules name.
        let text = self.lex.text.whole().unwrap_or_default();
        let depth = self.lex.nest.depth() as usize;
        if self.lex.is_key {
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
