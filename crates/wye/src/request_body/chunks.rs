//! Held chunks, and splicing a value into a body kept as chunks: an alias
//! into the held start, or a provider's model id into a body handed back
//! for another attempt. Chunks are sliced, never copied.

use std::{collections::VecDeque, ops::Range};

use bytes::Bytes;

/// Chunks held in order. The first three are stored inline: Claude Code
/// sends `model` first, so most holds keep one chunk, and splicing an alias
/// into it makes three (before, the new name, after), none allocating a queue.
#[derive(Default)]
pub(super) struct Chunks {
    inline: [Option<Bytes>; 3],
    /// Index of the next inline chunk to pop.
    next: usize,
    /// Inline chunks still held.
    held: usize,
    /// Chunks beyond the inline ones, in order after them.
    rest: VecDeque<Bytes>,
}

impl Chunks {
    pub(super) fn push(&mut self, chunk: Bytes) {
        // Inline only while nothing has overflowed, so order is kept.
        match self.inline.get_mut(self.next + self.held) {
            Some(slot) if self.rest.is_empty() => {
                *slot = Some(chunk);
                self.held += 1;
            }
            _ => self.rest.push_back(chunk),
        }
    }

    pub(super) fn pop(&mut self) -> Option<Bytes> {
        if self.held == 0 {
            return self.rest.pop_front();
        }
        let chunk = self.inline.get_mut(self.next).and_then(Option::take);
        self.next += 1;
        self.held -= 1;
        if self.held == 0 && self.rest.is_empty() {
            self.next = 0;
        }
        chunk
    }

    pub(super) fn is_empty(&self) -> bool {
        self.held == 0 && self.rest.is_empty()
    }

    pub(super) fn bytes(&self) -> u64 {
        self.inline
            .iter()
            .flatten()
            .chain(&self.rest)
            .map(|c| c.len() as u64)
            .sum()
    }
}

/// Replaces the bytes at `span` of a body, given as its chunks in order from
/// offset 0, with `value`, passing the result to `push`: every other byte is
/// kept, and chunks are sliced, not copied. `value` is moved in, not cloned:
/// the first clone of a freshly built `Bytes` allocates.
pub(crate) fn splice_chunks(
    chunks: impl Iterator<Item = Bytes>,
    span: &Range<u64>,
    value: Bytes,
    mut push: impl FnMut(Bytes),
) {
    let mut value = Some(value);
    let mut start = 0u64;
    for chunk in chunks {
        let end = start + chunk.len() as u64;
        if end <= span.start || start >= span.end {
            // An empty span on a chunk boundary (`"model":""` cut between
            // its quotes) overlaps no chunk: the value goes before the
            // first chunk past it.
            if start >= span.start
                && let Some(value) = value.take()
            {
                push(value);
            }
            push(chunk);
        } else {
            let head = span.start.saturating_sub(start) as usize;
            if head > 0 {
                push(chunk.slice(..head));
            }
            if span.start >= start
                && let Some(value) = value.take()
            {
                push(value);
            }
            let tail = (span.end.min(end) - start) as usize;
            if tail < chunk.len() {
                push(chunk.slice(tail..));
            }
        }
        start = end;
    }
    // An empty span at the very end (failover's sent bytes ending inside
    // `"model":""`): the value goes last.
    if start == span.start
        && let Some(value) = value.take()
    {
        push(value);
    }
}
