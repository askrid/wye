//! Cache-miss diagnostics: prompt hashes of forwarded request bodies, keyed
//! and written into usage records.
//!
//! [`PromptBody`] feeds the request bytes to a [`PromptScanner`] as hyper
//! sends them upstream, without changing or delaying them, and stores the
//! hashes in a [`PromptSlot`] when the body ends. The usage record reads the
//! slot when the response ends; the writer task turns each hash into a keyed
//! tag ([`HashKey::tag`]) while serializing, off the request path.

use std::{
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

use bytes::Bytes;
use gateway_core::prompt::{
    HashKey, MAX_BREAKPOINTS, MESSAGE_PREFIXES, PromptHashes, PromptScanner,
};
use hyper::body::{Body, Frame, SizeHint};
use serde::{Serialize, Serializer};

/// Hashes of one request body, set once the whole body was read and could be
/// followed. Shared between the request body (in hyper's client) and the usage record.
pub(crate) type PromptSlot = Arc<OnceLock<PromptHashes>>;

/// Request body wrapper that hashes the prompt when diagnostics are on.
pub(crate) struct PromptBody<B> {
    inner: B,
    /// Boxed so the wrapper stays small when diagnostics are off.
    scan: Option<(Box<PromptScanner>, PromptSlot)>,
}

impl<B> PromptBody<B> {
    pub(crate) fn new(inner: B, slot: Option<PromptSlot>) -> Self {
        Self {
            inner,
            scan: slot.map(|slot| (Box::new(PromptScanner::new()), slot)),
        }
    }

    fn finish(&mut self) {
        if let Some((scanner, slot)) = self.scan.take()
            && let Some(hashes) = scanner.finish()
        {
            let _ = slot.set(hashes);
        }
    }
}

impl<B> Body for PromptBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        if let Some((scanner, _)) = this.scan.as_mut() {
            match &polled {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Some(data) = frame.data_ref() {
                        scanner.feed(data);
                    }
                    // With a known length, hyper stops polling once the body
                    // reports its end and never sees `Ready(None)`.
                    if this.inner.is_end_stream() {
                        this.finish();
                    }
                }
                Poll::Ready(None) => this.finish(),
                // An error or a dropped body leaves the slot empty: unknown.
                Poll::Ready(Some(Err(_))) | Poll::Pending => {}
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The `prompt` object of a usage record.
#[derive(Serialize)]
pub(crate) struct PromptRecord {
    /// `complete`, or `unknown` when the body was incomplete or could not be followed.
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<SectionRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SectionRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    breakpoint_count: Option<u32>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    breakpoints: Seq<BreakpointRecord, MAX_BREAKPOINTS>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    message_prefixes: Seq<PrefixRecord, MESSAGE_PREFIXES>,
}

#[derive(Serialize)]
struct SectionRecord {
    count: u32,
    hash: Tag,
}

#[derive(Serialize, Clone, Copy, Default)]
struct BreakpointRecord {
    section: &'static str,
    index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    block: Option<u32>,
    hash: Tag,
}

#[derive(Serialize, Clone, Copy, Default)]
struct PrefixRecord {
    index: u32,
    hash: Tag,
}

impl PromptRecord {
    /// Keys the hashes in `slot`; `unknown` if the slot is empty.
    pub(crate) fn new(slot: &PromptSlot, key: &HashKey) -> Self {
        let Some(hashes) = slot.get() else {
            return Self {
                status: "unknown",
                tools: None,
                system: None,
                messages: None,
                breakpoint_count: None,
                breakpoints: Seq::default(),
                message_prefixes: Seq::default(),
            };
        };
        let section = |s: Option<gateway_core::prompt::SectionHash>| {
            s.map(|s| SectionRecord {
                count: s.count,
                hash: Tag(key.tag(s.hash)),
            })
        };
        Self {
            status: "complete",
            tools: section(hashes.tools),
            system: section(hashes.system),
            messages: hashes.messages,
            breakpoint_count: Some(hashes.breakpoint_count),
            breakpoints: Seq::collect(hashes.breakpoints().iter().map(|b| BreakpointRecord {
                section: b.section.as_str(),
                index: b.index,
                block: b.block,
                hash: Tag(key.tag(b.hash)),
            })),
            message_prefixes: Seq::collect(hashes.message_prefixes().iter().map(|p| {
                PrefixRecord {
                    index: p.index,
                    hash: Tag(key.tag(p.hash)),
                }
            })),
        }
    }
}

/// A keyed hash, written as 16 hex digits (JSON numbers lose precision above 2^53).
#[derive(Clone, Copy, Default)]
struct Tag(u64);

impl Serialize for Tag {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // serde_json writes formatted strings straight into its output buffer.
        serializer.collect_str(&format_args!("{:016x}", self.0))
    }
}

/// Up to `N` items without allocating, serialized as a JSON array.
struct Seq<T, const N: usize> {
    items: [T; N],
    len: usize,
}

impl<T: Copy + Default, const N: usize> Default for Seq<T, N> {
    fn default() -> Self {
        Self {
            items: [T::default(); N],
            len: 0,
        }
    }
}

impl<T: Copy + Default, const N: usize> Seq<T, N> {
    fn collect(items: impl Iterator<Item = T>) -> Self {
        let mut seq = Self::default();
        for (slot, item) in seq.items.iter_mut().zip(items) {
            *slot = item;
            seq.len += 1;
        }
        seq
    }
}

impl<T, const N: usize> std::ops::Deref for Seq<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.items.get(..self.len).unwrap_or_default()
    }
}

impl<T: Serialize, const N: usize> Serialize for Seq<T, N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_fixed_width_hex() {
        assert_eq!(
            serde_json::to_string(&Tag(0x0123_4567_89ab_cdef)).unwrap(),
            "\"0123456789abcdef\""
        );
        assert_eq!(
            serde_json::to_string(&Tag(0)).unwrap(),
            "\"0000000000000000\""
        );
    }

    #[test]
    fn records_keyed_hashes_or_unknown() {
        let key = HashKey::new(&[7; 32]).unwrap();
        let slot = PromptSlot::default();
        let unknown = serde_json::to_value(PromptRecord::new(&slot, &key)).unwrap();
        assert_eq!(unknown, serde_json::json!({"status": "unknown"}));

        let mut scanner = PromptScanner::new();
        scanner.feed(br#"{"system":"s","messages":[{"role":"user","content":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}}]}]}"#);
        let hashes = scanner.finish().unwrap();
        let system = hashes.system.unwrap().hash;
        slot.set(hashes).unwrap();
        let record = serde_json::to_value(PromptRecord::new(&slot, &key)).unwrap();
        assert_eq!(record["status"], "complete");
        assert_eq!(record["messages"], 1);
        assert_eq!(record["breakpoint_count"], 1);
        assert_eq!(record["breakpoints"][0]["section"], "messages");
        assert_eq!(record["breakpoints"][0]["block"], 0);
        assert_eq!(record["message_prefixes"][0]["index"], 0);
        assert!(record.get("tools").is_none());
        // The tag, never the raw hash.
        assert_eq!(
            record["system"]["hash"],
            format!("{:016x}", key.tag(system))
        );
    }
}
