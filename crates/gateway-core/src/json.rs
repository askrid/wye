//! Byte-level JSON lexing shared by the request scanners: strings, escapes,
//! and whitespace, with chunk boundaries allowed anywhere.

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
    /// A string starts after its opening quote.
    #[inline(always)]
    pub(crate) fn start(&mut self) {
        self.backslash_odd = false;
    }

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
        let odd = run % 2 == 1;
        if run == at {
            odd != self.backslash_odd
        } else {
            odd
        }
    }
}

pub(crate) fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// Length of a string whose content starts `rest`, if it ends within
/// [`SHORT_STRING`] bytes without an escape.
pub(crate) fn short_string_len(rest: &[u8]) -> Option<usize> {
    for (at, &byte) in rest.iter().take(SHORT_STRING).enumerate() {
        match byte {
            b'"' => return Some(at),
            b'\\' => return None,
            _ => {}
        }
    }
    None
}
