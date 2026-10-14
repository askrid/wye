//! Prompt hashes for cache-miss diagnostics, read from an Anthropic Messages
//! request body.
//!
//! [`PromptScanner`] reads the forwarded request bytes without changing,
//! delaying, or buffering them. A structure scanner follows strings, escapes,
//! and nesting to find the top-level `tools`, `system`, and `messages` values,
//! and feeds their bytes to xxh3. Whitespace outside strings and every
//! `cache_control` member (with its separating comma) are left out, so
//! formatting and the marker Claude Code moves each turn do not change the
//! hashes. Memory is constant; chunk boundaries may fall anywhere.
//!
//! Each section is hashed separately, because its fields may arrive in any
//! order and combining them in cache order (`tools` → `system` → `messages`)
//! would need buffering. The telemetry backend compares them in that order.
//! Raw bytes are hashed, not a canonical form, so hashes compare between
//! bodies the same client wrote.
//!
//! Hashes are fingerprints of prompt content: export only [`HashKey::tag`]s.
//!
//! The scanner is not a JSON validator. It gives up (`None`) on what it cannot
//! follow: unbalanced or mismatched brackets, an unterminated string, a
//! repeated section, or an escaped key where the key name matters. A malformed
//! body that passes these checks is rejected by the provider, so its hashes do
//! not matter.

mod scanner;

use std::fmt;

use ring::hmac;
use xxhash_rust::xxh3::xxh3_64;

pub use scanner::PromptScanner;

/// Breakpoints recorded with their hashes; Anthropic accepts at most four.
pub const MAX_BREAKPOINTS: usize = 4;
/// Message prefix hashes kept, newest last. A turn usually appends two
/// messages, so the previous request's last message stays in this window.
pub const MESSAGE_PREFIXES: usize = 8;

/// A top-level request field that takes part in the prompt cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Tools,
    System,
    Messages,
}

impl Section {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::System => "system",
            Self::Messages => "messages",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Tools => 0,
            Self::System => 1,
            Self::Messages => 2,
        }
    }
}

/// Hash of one whole section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionHash {
    /// Array elements; 1 for a string `system`.
    pub count: u32,
    pub hash: u64,
}

/// A `cache_control` marker and the hash of its section up to the end of the
/// marked block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakpoint {
    pub section: Section,
    /// Element of the section array (`tools[index]`, `messages[index]`).
    pub index: u32,
    /// Content block within the message, for `messages` breakpoints.
    pub block: Option<u32>,
    pub hash: u64,
}

/// Hash of `messages[0..=index]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessagePrefix {
    pub index: u32,
    pub hash: u64,
}

const NO_BREAKPOINT: Breakpoint = Breakpoint {
    section: Section::Tools,
    index: 0,
    block: None,
    hash: 0,
};
const NO_PREFIX: MessagePrefix = MessagePrefix { index: 0, hash: 0 };

/// Unkeyed hashes of one request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHashes {
    pub tools: Option<SectionHash>,
    pub system: Option<SectionHash>,
    /// Number of messages.
    pub messages: Option<u32>,
    /// `cache_control` markers seen in tools, system blocks, and message content blocks.
    pub breakpoint_count: u32,
    breakpoints: [Breakpoint; MAX_BREAKPOINTS],
    breakpoints_len: usize,
    prefixes: [MessagePrefix; MESSAGE_PREFIXES],
    prefixes_len: usize,
    /// Ring position of the next prefix while scanning; 0 once finished.
    prefixes_next: usize,
}

impl PromptHashes {
    /// The first [`MAX_BREAKPOINTS`] breakpoints, in body order.
    pub fn breakpoints(&self) -> &[Breakpoint] {
        self.breakpoints
            .get(..self.breakpoints_len)
            .unwrap_or_default()
    }

    /// Hashes of the last [`MESSAGE_PREFIXES`] message prefixes, oldest first.
    pub fn message_prefixes(&self) -> &[MessagePrefix] {
        self.prefixes.get(..self.prefixes_len).unwrap_or_default()
    }

    const fn empty() -> Self {
        Self {
            tools: None,
            system: None,
            messages: None,
            breakpoint_count: 0,
            breakpoints: [NO_BREAKPOINT; MAX_BREAKPOINTS],
            breakpoints_len: 0,
            prefixes: [NO_PREFIX; MESSAGE_PREFIXES],
            prefixes_len: 0,
            prefixes_next: 0,
        }
    }

    /// Puts the message prefixes in order once scanning is done.
    fn finished(mut self) -> Self {
        if self.prefixes_len == MESSAGE_PREFIXES {
            self.prefixes.rotate_left(self.prefixes_next);
        }
        self.prefixes_next = 0;
        self
    }

    fn push_breakpoint(&mut self, breakpoint: Breakpoint) {
        if let Some(slot) = self.breakpoints.get_mut(self.breakpoints_len) {
            *slot = breakpoint;
            self.breakpoints_len += 1;
        }
    }

    fn push_prefix(&mut self, prefix: MessagePrefix) {
        if let Some(slot) = self.prefixes.get_mut(self.prefixes_next) {
            *slot = prefix;
        }
        self.prefixes_next = (self.prefixes_next + 1) % MESSAGE_PREFIXES;
        self.prefixes_len = (self.prefixes_len + 1).min(MESSAGE_PREFIXES);
    }
}

/// Whether requests of the Claude Code session `session_id` are diagnosed
/// when `percent` of sessions are.
///
/// A pure function of the session id, so every replica decides alike and a
/// sampled session keeps its complete request sequence, which explaining a
/// miss needs. Requests without a session id have nothing to be compared
/// with and are diagnosed only at 100%.
pub fn session_sampled(session_id: Option<&[u8]>, percent: u8) -> bool {
    match session_id {
        _ if percent >= 100 => true,
        Some(id) => xxh3_64(id) % 100 < u64::from(percent),
        None => false,
    }
}

/// Secret that keys exported hashes. Replicas share it, so their tags compare,
/// while a tag cannot be matched against a guessed prompt without it.
pub struct HashKey(hmac::Key);

impl HashKey {
    /// Shortest accepted secret, in bytes.
    pub const MIN_LEN: usize = 32;

    /// `None` when `secret` is shorter than [`Self::MIN_LEN`].
    pub fn new(secret: &[u8]) -> Option<Self> {
        (secret.len() >= Self::MIN_LEN).then(|| Self(hmac::Key::new(hmac::HMAC_SHA256, secret)))
    }

    /// HMAC-SHA256 of `hash`, truncated to 64 bits.
    pub fn tag(&self, hash: u64) -> u64 {
        let tag = hmac::sign(&self.0, &hash.to_le_bytes());
        let mut first = [0; 8];
        first.copy_from_slice(tag.as_ref().get(..8).unwrap_or(&[0; 8]));
        u64::from_le_bytes(first)
    }
}

impl fmt::Debug for HashKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HashKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests;
