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

pub use scanner::PromptScanner;

/// Breakpoints recorded with their hashes; Anthropic accepts at most four.
pub const MAX_BREAKPOINTS: usize = 4;
/// Message prefix hashes kept, newest last. A turn usually appends two
/// messages, so the previous request's last message stays in this window.
pub const MESSAGE_PREFIXES: usize = 8;

/// A top-level request field that takes part in the prompt cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Section {
    /// `tools`, hashed first by the provider.
    #[default]
    Tools,
    /// `system`.
    System,
    /// `messages`.
    Messages,
}

impl Section {
    /// The field's name, as records write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::System => "system",
            Self::Messages => "messages",
        }
    }
}

/// Hash of one whole section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionHash {
    /// Array elements; 1 for a string `system`.
    pub count: u32,
    /// The unkeyed xxh3 of the section's canonical bytes.
    pub hash: u64,
}

/// A `cache_control` marker and the hash of its section up to the end of the
/// marked block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Breakpoint {
    /// The section the marker is in.
    pub section: Section,
    /// Element of the section array (`tools[index]`, `messages[index]`).
    pub index: u32,
    /// Content block within the message, for `messages` breakpoints.
    pub block: Option<u32>,
    /// The unkeyed xxh3 of the section's bytes up to the marked block's end.
    pub hash: u64,
}

/// Hash of `messages[0..=index]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessagePrefix {
    /// The last message hashed.
    pub index: u32,
    /// The unkeyed xxh3 of `messages[0..=index]`.
    pub hash: u64,
}

/// Unkeyed hashes of one request body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptHashes {
    /// The whole `tools` array, when the body has one.
    pub tools: Option<SectionHash>,
    /// The whole `system` field, when the body has one.
    pub system: Option<SectionHash>,
    /// Number of messages.
    pub messages: Option<u32>,
    /// `cache_control` markers seen in tools, system blocks, and message
    /// content blocks.
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
        u64::from_le_bytes(tag.as_ref().first_chunk().copied().unwrap_or_default())
    }
}

impl fmt::Debug for HashKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HashKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests;
