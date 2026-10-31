//! Byte-level JSON lexing shared by the scanners: strings, escapes,
//! whitespace, and bracket nesting, with chunk boundaries allowed anywhere.

/// Strings shorter than this are scanned with a plain loop: keys and short
/// values end before a vector search would pay off.
const SHORT_STRING: usize = 16;

/// Finds the closing quote of a string whose bytes may arrive in several chunks.
#[derive(Debug, Default)]
pub(crate) struct StringEnd {
    /// The bytes of the current string seen so far end in an unpaired backslash.
    backslash_odd: bool,
}

impl StringEnd {
    /// Index of the closing quote in `segment`, the string's next bytes, if
    /// it is there.
    #[inline(always)]
    pub(crate) fn find(&mut self, segment: &[u8]) -> Option<usize> {
        for quote in memchr::memchr_iter(b'"', segment) {
            if !self.escaped_at(segment, quote) {
                self.backslash_odd = false;
                return Some(quote);
            }
        }
        self.backslash_odd = self.escaped_at(segment, segment.len());
        None
    }

    /// Whether `segment[at]` follows an odd number of backslashes. A run that
    /// reaches the segment start continues the previous chunk's.
    fn escaped_at(&self, segment: &[u8], at: usize) -> bool {
        let before = segment.get(..at).unwrap_or_default();
        let run = before.iter().rev().take_while(|&&b| b == b'\\').count();
        (run % 2 == 1) != (run == at && self.backslash_odd)
    }
}

pub(crate) fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// Length of a string whose content starts `rest`, if it ends within
/// [`SHORT_STRING`] bytes without an escape.
pub(crate) fn short_string_len(rest: &[u8]) -> Option<usize> {
    let at = rest
        .iter()
        .take(SHORT_STRING)
        .position(|&b| matches!(b, b'"' | b'\\'))?;
    (rest.get(at) == Some(&b'"')).then_some(at)
}

/// Open containers and whether each is an array, to match closing brackets.
/// Kinds are kept for the first [`Nesting::TRACKED`] levels; deeper JSON is
/// followed without the kind check.
#[derive(Debug, Default)]
pub(crate) struct Nesting {
    depth: u32,
    /// Bit `d` is set when the container at depth `d` is an array.
    arrays: u64,
}

impl Nesting {
    pub(crate) const TRACKED: u32 = 64;

    /// Open containers; 0 outside the top-level value.
    #[inline(always)]
    pub(crate) fn depth(&self) -> u32 {
        self.depth
    }

    /// Whether the container at `depth` is an array (unknown past
    /// [`Self::TRACKED`]: `false`).
    #[inline(always)]
    pub(crate) fn is_array(&self, depth: u32) -> bool {
        depth < Self::TRACKED && self.arrays & (1 << depth) != 0
    }

    /// Whether the innermost open container is an object: after a `,`
    /// there, the next string is a key.
    #[inline(always)]
    pub(crate) fn in_object(&self) -> bool {
        let depth = self.depth();
        depth > 0 && !self.is_array(depth)
    }

    pub(crate) fn open(&mut self, is_array: bool) {
        self.depth = self.depth.saturating_add(1);
        if self.depth < Self::TRACKED {
            if is_array {
                self.arrays |= 1 << self.depth;
            } else {
                self.arrays &= !(1 << self.depth);
            }
        }
    }

    /// Whether a closing bracket of kind `is_array` matches the open container.
    #[inline(always)]
    pub(crate) fn closes(&self, is_array: bool) -> bool {
        self.depth > 0 && (self.depth >= Self::TRACKED || self.is_array(self.depth) == is_array)
    }

    /// Closes the open container; `false` when the bracket does not match
    /// ([`Self::closes`]), and then nothing changes.
    #[inline(always)]
    pub(crate) fn close(&mut self, is_array: bool) -> bool {
        let matches = self.closes(is_array);
        if matches {
            self.depth -= 1;
        }
        matches
    }
}

/// String bytes kept in a fixed buffer; bytes that do not fit mark it long.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Kept<const N: usize> {
    buf: [u8; N],
    len: usize,
    long: bool,
}

impl<const N: usize> Default for Kept<N> {
    fn default() -> Self {
        Self {
            buf: [0; N],
            len: 0,
            long: false,
        }
    }
}

impl<const N: usize> Kept<N> {
    pub(crate) fn clear(&mut self) {
        self.len = 0;
        self.long = false;
    }

    /// Appends `bytes`, all or nothing.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        match self.buf.get_mut(self.len..self.len + bytes.len()) {
            Some(slot) => {
                slot.copy_from_slice(bytes);
                self.len += bytes.len();
            }
            None => self.long = true,
        }
    }

    /// Replaces the kept bytes; `bytes` too long leave them unchanged.
    pub(crate) fn set(&mut self, bytes: &[u8]) {
        if let Some(slot) = self.buf.get_mut(..bytes.len()) {
            slot.copy_from_slice(bytes);
            self.len = bytes.len();
        }
    }

    pub(crate) fn is_long(&self) -> bool {
        self.long
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or_default()
    }

    /// The bytes, unless some did not fit.
    pub(crate) fn whole(&self) -> Option<&[u8]> {
        (!self.long).then(|| self.bytes())
    }

    /// The bytes as text; `None` when empty or not UTF-8.
    pub(crate) fn text(&self) -> Option<&str> {
        std::str::from_utf8(self.bytes())
            .ok()
            .filter(|t| !t.is_empty())
    }
}

/// What [`Lexer::next`] stopped at.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Lexeme {
    /// The end of a string kept in [`Lexer::text`].
    Kept,
    /// A byte outside strings, not whitespace.
    Byte(u8),
}

/// A pull lexer for one JSON object, keeping the strings the reader asks for.
#[derive(Debug, Default)]
pub(crate) struct Lexer<const N: usize> {
    pub(crate) nest: Nesting,
    started: bool,
    /// Not a JSON object, or brackets that do not match.
    pub(crate) failed: bool,
    in_string: bool,
    string: StringEnd,
    /// Inside an object, the next string is a key.
    pub(crate) expect_key: bool,
    /// The current string is kept.
    capturing: bool,
    pub(crate) is_key: bool,
    /// The kept string. An escaped one reads as long: it matches nothing a
    /// reader looks for.
    pub(crate) text: Kept<N>,
}

impl<const N: usize> Lexer<N> {
    /// The next thing to act on at or after `*i`, which is left just past it;
    /// `None` when the chunk ends or the lexer failed.
    #[inline(always)]
    pub(crate) fn next(&mut self, chunk: &[u8], i: &mut usize) -> Option<Lexeme> {
        while *i < chunk.len() && !self.failed {
            if self.in_string {
                let rest = chunk.get(*i..).unwrap_or_default();
                let end = self.string.find(rest);
                if self.capturing {
                    let content = rest.get(..end.unwrap_or(rest.len())).unwrap_or_default();
                    if content.contains(&b'\\') {
                        self.text.long = true;
                    }
                    self.text.push(content);
                }
                let Some(quote) = end else {
                    *i = chunk.len();
                    return None;
                };
                *i += quote + 1;
                self.in_string = false;
                self.expect_key = false;
                if self.capturing {
                    return Some(Lexeme::Kept);
                }
                continue;
            }
            let &byte = chunk.get(*i)?;
            *i += 1;
            if is_space(byte) {
                continue;
            }
            if !self.started {
                self.started = true;
                if byte != b'{' {
                    self.failed = true;
                    return None;
                }
            }
            return Some(Lexeme::Byte(byte));
        }
        None
    }

    /// A string starts, after its opening quote.
    pub(crate) fn start_string(&mut self, capturing: bool) {
        self.text.clear();
        self.capturing = capturing;
        self.in_string = true;
    }

    /// Opens the container of bracket `byte`.
    pub(crate) fn open(&mut self, byte: u8) {
        self.nest.open(byte == b'[');
        self.expect_key = byte == b'{';
    }

    /// Closes the open container with bracket `byte`; a mismatch fails.
    pub(crate) fn close(&mut self, byte: u8) {
        self.failed = !self.nest.close(byte == b']');
        self.expect_key = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting_matches_bracket_kinds_and_tolerates_depth() {
        let mut nest = Nesting::default();
        assert!(!nest.closes(false), "nothing is open");
        nest.open(false);
        nest.open(true);
        assert!(!nest.close(false), "an array closed with a brace");
        assert_eq!(nest.depth(), 2);
        assert!(nest.close(true) && nest.close(false));
        assert_eq!(nest.depth(), 0);
        // Past the tracked depth, kinds are not checked, and nothing overflows.
        for _ in 0..100 {
            nest.open(true);
        }
        assert!(nest.close(false));
        assert_eq!(nest.depth(), 99);
    }
}
