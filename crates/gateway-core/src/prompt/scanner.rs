//! The structure scanner behind [`PromptScanner`].

use xxhash_rust::xxh3::Xxh3Default;

use crate::json::{Nesting, StringEnd, is_space, short_string_len};

use super::{Breakpoint, MessagePrefix, PromptHashes, Section, SectionHash};

/// Longest key kept for classification; longer keys match nothing we look for.
const KEY_CAP: usize = 16;
const CACHE_CONTROL: &[u8] = b"cache_control";
const QUOTED_CACHE_CONTROL: &[u8] = b"\"cache_control\"";

/// How the key string being read is used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Capture {
    /// The key name does not matter.
    #[default]
    None,
    /// Kept to classify; hashed with the surrounding bytes.
    Plain,
    /// A member of an object that may hold `cache_control`: held back from the
    /// hash until the name is known.
    Deferred,
    /// Hashed in place; only checked for escapes. A deferred key becomes
    /// visible once it is too long to be `cache_control`.
    Visible,
}

/// Incremental hasher for one request body.
#[derive(Default)]
pub struct PromptScanner {
    hasher: Xxh3Default,
    out: PromptHashes,
    failed: bool,
    started: bool,
    ended: bool,

    // Lexical state.
    nest: Nesting,
    in_string: bool,
    string_is_key: bool,
    string: StringEnd,
    /// Inside an object, the next string is a key.
    expect_key: bool,

    // Key capture.
    capture: Capture,
    key: [u8; KEY_CAP],
    key_len: usize,
    key_long: bool,
    key_escaped: bool,

    // Section state.
    /// Section named by the current top-level key.
    member: Option<Section>,
    awaiting_value: bool,
    /// Section whose value is being hashed.
    section: Option<Section>,
    section_is_array: bool,
    seen: [bool; 3],
    element_open: bool,
    elements: u32,
    /// The current message member is `content`.
    in_content: bool,
    /// Index of the current content block.
    block: u32,
    /// Depth of the object whose `cache_control` member is being skipped; 0 when none.
    skip_depth: u32,
    /// Depth around the open value whose structure does not matter (tool
    /// schemas, tool inputs, marker values); 0 when none. Such values are
    /// lexed only for nesting and strings.
    raw_floor: u32,
    /// A member of the open block object was hashed, so the next one needs a comma.
    member_hashed: bool,
    /// That comma was held back until the next key is known.
    comma_held: bool,
    /// The open block object holds a `cache_control` member.
    marked: bool,
}

impl PromptScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        // Start of the bytes not yet hashed or dropped.
        let mut run = 0;
        let mut i = 0;
        while i < chunk.len() && !self.failed {
            if self.raw_floor != 0 && !self.in_string {
                i = self.feed_raw(chunk, i, &mut run);
                continue;
            }
            if self.in_string {
                // Most bytes are string content: jump to the closing quote.
                let end = self
                    .string
                    .find(chunk.get(i..).unwrap_or_default())
                    .map(|quote| i + quote);
                self.capture_key(chunk, i, end.unwrap_or(chunk.len()), &mut run);
                match end {
                    Some(quote) => {
                        self.in_string = false;
                        self.end_string(chunk, quote, &mut run);
                        i = quote + 1;
                    }
                    None => i = chunk.len(),
                }
                continue;
            }
            i = match chunk.get(i) {
                Some(&byte) => self.structural(chunk, i, byte, &mut run),
                None => i + 1,
            };
        }
        if !self.failed {
            self.flush(chunk, &mut run, chunk.len());
        }
    }

    /// The hashes, or `None` when the body was incomplete or could not be followed.
    pub fn finish(self) -> Option<PromptHashes> {
        (!self.failed && self.ended).then(|| self.out.finished())
    }

    /// Lexes inside a raw value from `i`; returns where normal scanning resumes.
    fn feed_raw(&mut self, chunk: &[u8], mut i: usize, run: &mut usize) -> usize {
        while let Some(&byte) = chunk.get(i) {
            match byte {
                b'"' => {
                    let rest = chunk.get(i + 1..).unwrap_or_default();
                    match short_string_len(rest) {
                        Some(len) => i += len + 1,
                        None => {
                            self.in_string = true;
                            self.string.start();
                            return i + 1;
                        }
                    }
                }
                b'{' | b'[' => self.push(byte == b'['),
                b'}' | b']' => {
                    if !self.pop(byte == b']') {
                        return i + 1;
                    }
                    if self.nest.depth() == self.raw_floor {
                        self.raw_floor = 0;
                        self.expect_key = false;
                        return i + 1;
                    }
                }
                byte if is_space(byte) => {
                    self.flush(chunk, run, i);
                    *run = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        i
    }

    #[inline(always)]
    fn push(&mut self, is_array: bool) {
        self.nest.open(is_array);
    }

    /// Leaves the open container; `false` (and the scan fails) if `is_array`
    /// does not match it.
    #[inline(always)]
    fn pop(&mut self, is_array: bool) -> bool {
        let closed = self.nest.close(is_array);
        self.failed |= !closed;
        closed
    }

    /// Deepest level whose structure matters in the current position.
    fn tracked_depth(&self) -> u32 {
        match self.section {
            None => 1,
            Some(Section::Tools | Section::System) => 3,
            Some(Section::Messages) if self.in_content => 5,
            Some(Section::Messages) => 3,
        }
    }

    #[inline(always)]
    fn hashing(&self) -> bool {
        self.section.is_some() && self.skip_depth == 0 && self.capture != Capture::Deferred
    }

    /// Hashes `chunk[run..end]` if hashing is on, and moves `run` to `end`.
    #[inline(always)]
    fn flush(&mut self, chunk: &[u8], run: &mut usize, end: usize) {
        if self.hashing()
            && let Some(bytes) = chunk.get(*run..end)
        {
            self.hasher.update(bytes);
        }
        *run = end;
    }

    #[inline(always)]
    fn is_array(&self, depth: u32) -> bool {
        self.nest.is_array(depth)
    }

    /// Whether the open object is a block that may hold `cache_control`: a tool
    /// definition, a system block, or a message content block. Blocks never
    /// nest, so one set of block flags suffices.
    #[inline(always)]
    fn in_block(&self) -> bool {
        match (self.section, self.nest.depth()) {
            (Some(Section::Tools | Section::System), 3) => self.is_array(2) && !self.is_array(3),
            (Some(Section::Messages), 5) => {
                self.in_content && self.is_array(4) && !self.is_array(5)
            }
            _ => false,
        }
    }

    #[inline(always)]
    fn in_content_array(&self) -> bool {
        self.section == Some(Section::Messages)
            && self.nest.depth() == 4
            && self.in_content
            && self.is_array(4)
    }

    /// Keeps the key bytes `chunk[from..to]` when the name matters.
    fn capture_key(&mut self, chunk: &[u8], from: usize, to: usize, run: &mut usize) {
        if self.capture == Capture::None {
            return;
        }
        let bytes = chunk.get(from..to).unwrap_or_default();
        if bytes.contains(&b'\\') {
            self.key_escaped = true;
        }
        if self.capture == Capture::Visible {
            return;
        }
        let Some(slot) = self.key.get_mut(self.key_len..self.key_len + bytes.len()) else {
            self.key_long = true;
            if self.capture == Capture::Deferred {
                // Too long for `cache_control`: hash what was held back and
                // let the rest of the key flow through `run`.
                self.release_key();
                self.capture = Capture::Visible;
                *run = from;
            }
            return;
        };
        slot.copy_from_slice(bytes);
        self.key_len += bytes.len();
    }

    fn release_comma(&mut self) {
        if std::mem::take(&mut self.comma_held) {
            self.hasher.update(b",");
        }
    }

    /// Hashes a held-back key's comma, opening quote, and kept bytes.
    fn release_key(&mut self) {
        self.release_comma();
        self.member_hashed = true;
        self.hasher.update(b"\"");
        self.hasher
            .update(self.key.get(..self.key_len).unwrap_or_default());
    }

    /// Handles a byte outside strings; returns the index after the bytes consumed.
    #[inline(always)]
    fn structural(&mut self, chunk: &[u8], i: usize, byte: u8, run: &mut usize) -> usize {
        if is_space(byte) {
            self.flush(chunk, run, i);
            *run = i + 1;
            return i + 1;
        }
        if self.ended || (!self.started && byte != b'{') {
            self.failed = true;
            return i + 1;
        }
        match byte {
            b'"' => return self.open_string(chunk, i, run),
            b'{' | b'[' => self.open(chunk, i, byte == b'[', run),
            b'}' | b']' => self.close(chunk, i, byte == b']', run),
            b',' => self.comma(chunk, i, run),
            b':' => {
                if self.nest.depth() == 1 && self.member.is_some() {
                    self.awaiting_value = true;
                }
            }
            _ => self.value_start(chunk, i, run),
        }
        i + 1
    }

    /// Called at the first byte of every value, and at each byte of a bare scalar.
    #[inline(always)]
    fn value_start(&mut self, chunk: &[u8], i: usize, run: &mut usize) {
        if self.awaiting_value && self.nest.depth() == 1 {
            self.awaiting_value = false;
            self.flush(chunk, run, i);
            self.section = self.member;
            self.section_is_array = chunk.get(i) == Some(&b'[');
            self.hasher.reset();
            self.elements = 0;
            self.element_open = false;
        } else if self.nest.depth() == 2 && self.section.is_some() {
            self.element_open = true;
        }
    }

    /// Starts a string; returns the index after the bytes consumed.
    #[inline(always)]
    fn open_string(&mut self, chunk: &[u8], i: usize, run: &mut usize) -> usize {
        let rest = chunk.get(i + 1..).unwrap_or_default();
        // Most keys and small values are short and escape-free: finish them here.
        let short = short_string_len(rest);
        if self.expect_key {
            self.expect_key = false;
            let role = if self.nest.depth() == 1
                || (self.section == Some(Section::Messages)
                    && self.nest.depth() == 3
                    && !self.is_array(3))
            {
                Capture::Plain
            } else if self.skip_depth == 0 && self.in_block() {
                Capture::Deferred
            } else {
                Capture::None
            };
            if let Some(len) = short {
                let key = rest.get(..len).unwrap_or_default();
                let after = i + len + 2;
                match role {
                    Capture::Plain => self.classify(Some(key)),
                    Capture::Deferred if key == CACHE_CONTROL => {
                        self.flush(chunk, run, i);
                        self.start_marker();
                        *run = after;
                    }
                    Capture::Deferred => self.hash_member_in_place(chunk, i, run),
                    Capture::None | Capture::Visible => {}
                }
                return after;
            }
            self.string_is_key = true;
            self.key_len = 0;
            self.key_long = false;
            self.key_escaped = false;
            self.capture = match role {
                Capture::Deferred if may_be_marker(chunk.get(i..).unwrap_or_default()) => {
                    self.flush(chunk, run, i);
                    Capture::Deferred
                }
                Capture::Deferred => {
                    // The key is visibly another name; hash it in place.
                    self.hash_member_in_place(chunk, i, run);
                    Capture::Visible
                }
                role => role,
            };
        } else {
            self.value_start(chunk, i, run);
            if let Some(len) = short
                && !(self.nest.depth() == 1 && self.section.is_some())
            {
                return i + len + 2;
            }
        }
        self.in_string = true;
        self.string.start();
        i + 1
    }

    /// A block member other than `cache_control` starts at `i`.
    fn hash_member_in_place(&mut self, chunk: &[u8], i: usize, run: &mut usize) {
        if self.comma_held {
            self.flush(chunk, run, i);
            self.release_comma();
        }
        self.member_hashed = true;
    }

    /// The open block's `cache_control` member starts: skip it.
    fn start_marker(&mut self) {
        self.skip_depth = self.nest.depth();
        self.marked = true;
        self.comma_held = false;
        self.out.breakpoint_count = self.out.breakpoint_count.saturating_add(1);
    }

    /// Acts on a key whose name matters; `None` for a key too long to match.
    fn classify(&mut self, key: Option<&[u8]>) {
        if self.nest.depth() == 1 {
            self.member = match key {
                Some(b"tools") => Some(Section::Tools),
                Some(b"system") => Some(Section::System),
                Some(b"messages") => Some(Section::Messages),
                _ => None,
            };
            if let Some(seen) = self.member.and_then(|m| self.seen.get_mut(m as usize)) {
                if *seen {
                    self.failed = true;
                }
                *seen = true;
            }
        } else {
            self.in_content = key == Some(b"content");
        }
    }

    fn end_string(&mut self, chunk: &[u8], quote: usize, run: &mut usize) {
        if !self.string_is_key {
            if self.nest.depth() == 1 && self.section.is_some() {
                // A string `system` ends here.
                self.flush(chunk, run, quote + 1);
                self.finish_section();
            }
            return;
        }
        self.string_is_key = false;
        let capture = std::mem::replace(&mut self.capture, Capture::None);
        if capture == Capture::None {
            return;
        }
        if self.key_escaped {
            // An escape could spell a name we look for.
            self.failed = true;
            return;
        }
        // A copy, so `classify` can borrow `self` mutably.
        let kept = self.key;
        let key = (!self.key_long).then(|| kept.get(..self.key_len).unwrap_or_default());
        match capture {
            Capture::Deferred if key == Some(CACHE_CONTROL) => {
                self.start_marker();
                *run = quote + 1;
            }
            Capture::Deferred => {
                self.release_key();
                self.hasher.update(b"\"");
                *run = quote + 1;
            }
            Capture::Plain => self.classify(key),
            Capture::None | Capture::Visible => {}
        }
    }

    #[inline(always)]
    fn open(&mut self, chunk: &[u8], i: usize, is_array: bool, run: &mut usize) {
        if self.started {
            self.value_start(chunk, i, run);
        }
        self.started = true;
        self.push(is_array);
        if self.nest.depth() > self.tracked_depth() {
            self.raw_floor = self.nest.depth() - 1;
            return;
        }
        self.expect_key = !is_array;
        if self.in_content_array() {
            self.block = 0;
        }
        if self.in_block() {
            self.member_hashed = false;
            self.comma_held = false;
            self.marked = false;
        }
    }

    #[inline(always)]
    fn close(&mut self, chunk: &[u8], i: usize, is_array: bool, run: &mut usize) {
        if !self.nest.closes(is_array) {
            self.failed = true;
            return;
        }
        if self.skip_depth == self.nest.depth() {
            // The skipped `cache_control` was the object's last member.
            self.skip_depth = 0;
            *run = i;
        }
        if self.section.is_some() {
            match self.nest.depth() {
                // A bare scalar section value ends at the top-level `}`.
                1 => {
                    self.flush(chunk, run, i);
                    self.finish_section();
                }
                2 if self.element_open => self.finish_element(chunk, run, i),
                _ => {}
            }
        }
        let marked = self.in_block() && std::mem::take(&mut self.marked);
        self.nest.close(is_array);
        self.expect_key = false;
        if marked {
            self.flush(chunk, run, i + 1);
            self.record_breakpoint();
        }
        if self.nest.depth() == 1 && self.section.is_some() {
            self.flush(chunk, run, i + 1);
            self.finish_section();
        }
        if self.nest.depth() == 0 {
            self.ended = true;
        }
    }

    #[inline(always)]
    fn comma(&mut self, chunk: &[u8], i: usize, run: &mut usize) {
        if self.section.is_some() {
            match self.nest.depth() {
                // A bare scalar section value ends at the next top-level member.
                1 => {
                    self.flush(chunk, run, i);
                    self.finish_section();
                }
                2 if self.element_open => self.finish_element(chunk, run, i),
                _ => {}
            }
        }
        if self.skip_depth != 0 && self.skip_depth == self.nest.depth() {
            // The comma after a skipped `cache_control` member separates the
            // hashed members around it, if there are any.
            self.skip_depth = 0;
            *run = i + 1;
            self.comma_held = self.member_hashed;
        } else if self.in_block() {
            let next = chunk.get(i + 1..).unwrap_or_default();
            if next.first() != Some(&b'"') || may_be_marker(next) {
                // Hashed with the next member, unless that is `cache_control`.
                self.flush(chunk, run, i);
                *run = i + 1;
                self.comma_held = true;
            }
        }
        if self.in_content_array() {
            self.block = self.block.saturating_add(1);
        }
        self.expect_key = self.nest.in_object();
    }

    /// Ends the section element whose last byte precedes `end`.
    fn finish_element(&mut self, chunk: &[u8], run: &mut usize, end: usize) {
        self.element_open = false;
        let index = self.elements;
        self.elements = self.elements.saturating_add(1);
        if self.section == Some(Section::Messages) {
            self.flush(chunk, run, end);
            let hash = self.hasher.digest();
            self.out.push_prefix(MessagePrefix { index, hash });
        }
    }

    fn finish_section(&mut self) {
        let Some(section) = self.section.take() else {
            return;
        };
        let count = if self.section_is_array {
            self.elements
        } else {
            1
        };
        let hash = SectionHash {
            count,
            hash: self.hasher.digest(),
        };
        match section {
            Section::Tools => self.out.tools = Some(hash),
            Section::System => self.out.system = Some(hash),
            Section::Messages => self.out.messages = Some(count),
        }
    }

    /// Records the hash through the block object that just closed.
    fn record_breakpoint(&mut self) {
        let Some(section) = self.section else {
            return;
        };
        let breakpoint = Breakpoint {
            section,
            index: self.elements,
            block: (section == Section::Messages).then_some(self.block),
            hash: self.hasher.digest(),
        };
        self.out.push_breakpoint(breakpoint);
    }
}

/// Whether a key starting at `rest` may be `"cache_control"`: it is, or the
/// chunk ends before it can tell.
fn may_be_marker(rest: &[u8]) -> bool {
    let n = rest.len().min(QUOTED_CACHE_CONTROL.len());
    rest.get(..n) == QUOTED_CACHE_CONTROL.get(..n)
}
