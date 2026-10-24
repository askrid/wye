//! Byte-level JSON lexing shared by the request scanners: strings, escapes,
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

    #[inline(always)]
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
