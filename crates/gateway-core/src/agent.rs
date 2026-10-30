//! Coding agents' per-request signals: Claude Code's agent structure and
//! gateway hint headers (request class, agent type, compaction, prompt id),
//! and Codex's subagent and thread headers, mapped onto the same fields.
//!
//! Values are client-supplied. They only ever label telemetry, never
//! authorize anything, and are parsed into closed vocabularies or bounded
//! tokens before they become attributes, so a client cannot grow metric
//! series or inject unbounded text.

use std::borrow::Cow;

/// Longest identifier or agent type kept; longer values are dropped.
pub const MAX_TOKEN_BYTES: usize = 128;

/// What kind of request this is (`x-claude-code-request-class`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// A turn of the main conversation.
    Main,
    /// A turn of a subagent.
    Subagent,
    /// An agent running inside a workflow.
    Workflow,
    /// The summarization request that compacts a conversation.
    Compaction,
    /// Side requests such as session titles, classifiers, and summaries.
    Auxiliary,
    /// A value this version does not know.
    Other,
}

impl RequestClass {
    /// `None` when the header is absent.
    pub fn parse(value: Option<&[u8]>) -> Option<Self> {
        Some(match value? {
            b"main" => Self::Main,
            b"subagent" => Self::Subagent,
            b"workflow" => Self::Workflow,
            b"compaction" => Self::Compaction,
            b"auxiliary" => Self::Auxiliary,
            _ => Self::Other,
        })
    }

    /// The class of a Codex request from its `x-openai-subagent` value.
    /// Codex sends the header for every subagent request (review,
    /// compaction, memory consolidation, spawned threads) and never for the
    /// main thread, so its absence means `main`, but only from Codex
    /// (`is_codex`: its `originator` header): other OpenAI clients send
    /// neither.
    pub fn from_codex_subagent(value: Option<&[u8]>, is_codex: bool) -> Option<Self> {
        match value {
            Some(b"collab_spawn") => Some(Self::Subagent),
            Some(b"compact") => Some(Self::Compaction),
            Some(b"review" | b"memory_consolidation") => Some(Self::Auxiliary),
            Some(_) => Some(Self::Other),
            None => is_codex.then_some(Self::Main),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Subagent => "subagent",
            Self::Workflow => "workflow",
            Self::Compaction => "compaction",
            Self::Auxiliary => "auxiliary",
            Self::Other => "other",
        }
    }
}

/// What triggered a compaction (`x-claude-code-compaction` on the
/// summarization request, `x-claude-code-context-compacted` on the first main
/// request after it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compaction {
    /// The context window approached capacity, or idle compaction.
    Auto,
    /// `/compact`.
    Manual,
    /// The API rejected a request as too long.
    Reactive,
    /// A value this version does not know.
    Other,
}

impl Compaction {
    /// `None` when the header is absent.
    pub fn parse(value: Option<&[u8]>) -> Option<Self> {
        Some(match value? {
            b"auto" => Self::Auto,
            b"manual" => Self::Manual,
            b"reactive" => Self::Reactive,
            _ => Self::Other,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Reactive => "reactive",
            Self::Other => "other",
        }
    }
}

/// An identifier or agent type usable as a telemetry attribute: 1 to
/// [`MAX_TOKEN_BYTES`] bytes of letters, digits, `-`, `_`, `.`, or `:`.
/// Claude Code sends UUIDs, built-in agent type names, and name-based
/// teammate ids, which all fit; anything else is dropped rather than escaped.
pub fn token(value: &[u8]) -> Option<&str> {
    let valid = !value.is_empty()
        && value.len() <= MAX_TOKEN_BYTES
        && value
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if valid {
        std::str::from_utf8(value).ok()
    } else {
        None
    }
}

/// Most tool entries read from `x-claude-code-prev-tool-durations`; Claude
/// Code sends at most 32.
pub const MAX_TOOL_ENTRIES: usize = 32;

/// The tool calls whose results a request carries, from
/// `x-claude-code-prev-tool-durations` (`Bash=742;Read=9`): tool name and run
/// time in milliseconds, in the order Claude Code collected the results.
/// Names are percent-decoded and kept only when they are a [`token`];
/// malformed entries are skipped, and at most [`MAX_TOOL_ENTRIES`] are read.
pub fn tool_durations(value: &[u8]) -> impl Iterator<Item = (Cow<'_, str>, u64)> {
    value
        .split(|&b| b == b';')
        .take(MAX_TOOL_ENTRIES)
        .filter_map(|entry| {
            let eq = entry.iter().position(|&b| b == b'=')?;
            let (name, ms) = (entry.get(..eq)?, entry.get(eq + 1..)?);
            let ms = std::str::from_utf8(ms).ok()?.parse::<u64>().ok()?;
            let name = percent_decode(name)?;
            token(name.as_bytes())?;
            Some((name, ms))
        })
}

/// `None` for a malformed escape or a result that is not UTF-8.
fn percent_decode(raw: &[u8]) -> Option<Cow<'_, str>> {
    if !raw.contains(&b'%') {
        return std::str::from_utf8(raw).ok().map(Cow::Borrowed);
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut bytes = raw.iter();
    while let Some(&b) = bytes.next() {
        if b == b'%' {
            let hex = [*bytes.next()?, *bytes.next()?];
            let hex = std::str::from_utf8(&hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok().map(Cow::Owned)
}

/// Why a request read nothing from the prompt cache although it wrote to it,
/// as far as one request can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMiss {
    /// The compaction summarization request itself.
    Compaction,
    /// The first main-conversation request after a compaction: the old
    /// prefix is no longer used.
    AfterCompaction,
    /// No signal explains it. Also the label of a session's first request,
    /// which one request cannot recognize; the telemetry backend excludes
    /// those by session.
    Unexplained,
}

impl CacheMiss {
    /// `None` unless the provider reported a cache write and no cache read.
    pub fn classify(
        cache_read_tokens: Option<u64>,
        cache_creation_tokens: Option<u64>,
        compaction: Option<Compaction>,
        context_compacted: Option<Compaction>,
    ) -> Option<Self> {
        let missed = cache_read_tokens == Some(0) && cache_creation_tokens.is_some_and(|n| n > 0);
        missed.then(|| {
            if compaction.is_some() {
                Self::Compaction
            } else if context_compacted.is_some() {
                Self::AfterCompaction
            } else {
                Self::Unexplained
            }
        })
    }

    /// Every reason, in the order metrics list them.
    pub const ALL: [Self; 3] = [Self::Compaction, Self::AfterCompaction, Self::Unexplained];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::AfterCompaction => "after_compaction",
            Self::Unexplained => "unexplained",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabularies_are_closed() {
        assert_eq!(RequestClass::parse(None), None);
        assert_eq!(
            RequestClass::parse(Some(b"subagent")),
            Some(RequestClass::Subagent)
        );
        assert_eq!(
            RequestClass::parse(Some(b"Main")),
            Some(RequestClass::Other),
            "case-sensitive, as sent"
        );
        assert_eq!(
            Compaction::parse(Some(b"reactive")),
            Some(Compaction::Reactive)
        );
        assert_eq!(
            Compaction::parse(Some(b"x".repeat(5000).as_slice())),
            Some(Compaction::Other)
        );
    }

    #[test]
    fn codex_subagent_kinds_map_to_request_classes() {
        let class = RequestClass::from_codex_subagent;
        assert_eq!(
            class(Some(b"collab_spawn"), true),
            Some(RequestClass::Subagent)
        );
        assert_eq!(
            class(Some(b"compact"), true),
            Some(RequestClass::Compaction)
        );
        assert_eq!(class(Some(b"review"), true), Some(RequestClass::Auxiliary));
        assert_eq!(
            class(Some(b"memory_consolidation"), false),
            Some(RequestClass::Auxiliary)
        );
        assert_eq!(
            class(Some(b"something-new"), true),
            Some(RequestClass::Other)
        );
        assert_eq!(class(None, true), Some(RequestClass::Main));
        assert_eq!(class(None, false), None, "not Codex: no inference");
    }

    #[test]
    fn tokens_are_bounded_and_plain() {
        assert_eq!(
            token(b"5f0c1a2e-0b7d-4a51-9a39-2b0f5c3d1e7a"),
            Some("5f0c1a2e-0b7d-4a51-9a39-2b0f5c3d1e7a")
        );
        assert_eq!(token(b"general-purpose"), Some("general-purpose"));
        assert_eq!(token(b"team:reviewer"), Some("team:reviewer"));
        for bad in [&b""[..], b"a b", b"a\"b", b"\xc3\xa9", &[b'a'; 129]] {
            assert_eq!(token(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn tool_durations_parse_bounded_valid_entries() {
        let parsed: Vec<_> =
            tool_durations(b"Bash=742;Read=9;mcp__gh__issue=12;bad;Web%20Fetch=3;Edit=x;T%3Ax=4")
                .collect();
        assert_eq!(
            parsed,
            [
                (Cow::from("Bash"), 742),
                (Cow::from("Read"), 9),
                (Cow::from("mcp__gh__issue"), 12),
                (Cow::from("T:x"), 4),
            ],
            "the decoded space fails the token check; malformed entries are skipped"
        );
        let many = "A=1;".repeat(40);
        assert_eq!(tool_durations(many.as_bytes()).count(), MAX_TOOL_ENTRIES);
        assert_eq!(tool_durations(b"A%2=1;B%zz=1;=5").count(), 0);
    }

    proptest::proptest! {
        #[test]
        fn tool_durations_never_yield_unbounded_or_unsafe_names(
            raw in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512),
        ) {
            let parsed: Vec<_> = tool_durations(&raw).collect();
            proptest::prop_assert!(parsed.len() <= MAX_TOOL_ENTRIES);
            for (name, _) in parsed {
                proptest::prop_assert!(token(name.as_bytes()).is_some());
            }
        }
    }

    #[test]
    fn cache_misses_are_explained_by_compaction_signals() {
        let auto = Some(Compaction::Auto);
        assert_eq!(CacheMiss::classify(Some(5), Some(9), None, None), None);
        assert_eq!(CacheMiss::classify(Some(0), Some(0), None, None), None);
        assert_eq!(CacheMiss::classify(None, None, None, None), None);
        assert_eq!(
            CacheMiss::classify(Some(0), Some(9), auto, None),
            Some(CacheMiss::Compaction)
        );
        assert_eq!(
            CacheMiss::classify(Some(0), Some(9), None, auto),
            Some(CacheMiss::AfterCompaction)
        );
        assert_eq!(
            CacheMiss::classify(Some(0), Some(9), None, None),
            Some(CacheMiss::Unexplained)
        );
    }
}
