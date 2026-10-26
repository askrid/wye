//! Model permissions and output-token ceilings: the request's `model` (and
//! `max_tokens`, when it arrives in time) is checked before anything is sent
//! upstream.
//!
//! [`hold_until_model`] reads the request body only until its top-level
//! `model` value has been read. Claude Code sends `model` first, so usually
//! only the first chunk waits. The held bytes count against a process-wide
//! budget (`server.max_held_request_bytes`). When the model is allowed,
//! [`RequestBody`] replays the held chunks and streams the rest, and keeps
//! checking it: a second top-level `model` key, or a `max_tokens` above the
//! ceiling that arrives after the hold, fails the body, which aborts the
//! upstream request before the provider sees a complete request.

use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use gateway_core::policy::{PolicyError, PolicyScanner};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, SizeHint};

use crate::{
    gauge::{Gauge, Held},
    upstream::BoxError,
};

/// Why a request was refused while its model was read.
#[derive(Debug)]
pub(crate) enum HoldError {
    /// The body's `model` cannot be checked.
    Model(PolicyError),
    /// The process-wide hold budget is used up.
    Overloaded,
    /// The body exceeded the request size limit while being held.
    TooLarge,
    /// The client's body failed (for example, the connection dropped).
    Body,
}

/// Chunks held in order. The first three are stored inline: Claude Code
/// sends `model` first, so most holds keep one chunk, and splicing an alias
/// into it makes three (before, the new name, after), none allocating a queue.
#[derive(Default)]
struct Chunks {
    inline: [Option<Bytes>; 3],
    /// Index of the next inline chunk to pop.
    next: usize,
    /// Inline chunks still held.
    held: usize,
    /// Chunks beyond the inline ones, in order after them.
    rest: VecDeque<Bytes>,
}

impl Chunks {
    fn push(&mut self, chunk: Bytes) {
        // Inline only while nothing has overflowed, so order is kept.
        match self.inline.get_mut(self.next + self.held) {
            Some(slot) if self.rest.is_empty() => {
                *slot = Some(chunk);
                self.held += 1;
            }
            _ => self.rest.push_back(chunk),
        }
    }

    fn pop(&mut self) -> Option<Bytes> {
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

    fn is_empty(&self) -> bool {
        self.held == 0 && self.rest.is_empty()
    }

    fn bytes(&self) -> u64 {
        self.inline
            .iter()
            .flatten()
            .chain(&self.rest)
            .map(|c| c.len() as u64)
            .sum()
    }
}

/// What a held request carries until its body is fully sent.
struct Hold {
    chunks: Chunks,
    /// Bytes of `chunks`, counted against the hold budget.
    budget: Held,
    scanner: PolicyScanner,
    /// The whole body is in `chunks`.
    ended: bool,
    /// The request's `max_tokens` ceiling, checked again as the rest streams.
    token_limit: Option<u64>,
    /// Whether the rest of the body is checked as it streams (a repeated
    /// `model`, a late `max_tokens`); off when no permission or ceiling applies.
    check_rest: bool,
}

/// A `max_tokens` above the request's ceiling, found after forwarding began.
#[derive(Debug)]
pub(crate) struct OverTokenLimit {
    pub(crate) requested: u64,
    pub(crate) limit: u64,
}

impl std::fmt::Display for OverTokenLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "max_tokens {} exceeds this credential's limit of {}",
            self.requested, self.limit
        )
    }
}

impl std::error::Error for OverTokenLimit {}

/// The start of a request body, read until its model was known.
///
/// Boxed, and kept boxed inside [`RequestBody`]: hyper's client channel
/// stores 32 requests inline per upstream connection, so every byte of the
/// body type costs 32 bytes per connection (on the plain HTTP/1.1 path,
/// per stream), restricted or not.
pub(crate) struct HeldStart(Box<Hold>);

impl HeldStart {
    /// The model named by the request.
    pub(crate) fn model(&self) -> Option<&str> {
        self.0.scanner.model()
    }

    /// The request's `max_tokens`, if it was read before the hold ended.
    pub(crate) fn max_tokens(&self) -> Option<u64> {
        self.0.scanner.max_tokens()
    }

    /// Streams the rest of the body unchecked. For a request held only to
    /// resolve an alias: the repeated-`model` check protects permissions, and
    /// lexing the rest costs CPU per uploaded MB.
    pub(crate) fn skip_rest_check(&mut self) {
        self.0.check_rest = false;
    }

    /// Replaces the value of `model` with `model` in the held bytes, keeping
    /// every other byte, and returns how many bytes longer the body became
    /// (negative when shorter). Chunks are sliced, not copied, and `model` is
    /// shared. `None` if the model's position is unknown.
    pub(crate) fn splice_model(&mut self, model: &Bytes) -> Option<i64> {
        let hold = &mut *self.0;
        let span = hold.scanner.model_span()?;
        let mut out = Chunks::default();
        let mut start = 0u64;
        while let Some(chunk) = hold.chunks.pop() {
            let end = start + chunk.len() as u64;
            if end <= span.start || start >= span.end {
                out.push(chunk);
            } else {
                // The value begins in the first chunk it overlaps; the hold
                // stopped right after it, so every byte of it is held.
                let head = span.start.saturating_sub(start) as usize;
                if head > 0 {
                    out.push(chunk.slice(..head));
                }
                if span.start >= start {
                    out.push(model.clone());
                }
                let tail = (span.end.min(end) - start) as usize;
                if tail < chunk.len() {
                    out.push(chunk.slice(tail..));
                }
            }
            start = end;
        }
        hold.chunks = out;
        let delta = model.len() as i64 - (span.end - span.start) as i64;
        let grown = delta.unsigned_abs() as usize;
        if delta > 0 {
            // At most MAX_MODEL_BYTES per request, so the hold limit is not
            // re-checked for it.
            hold.budget.try_grow(grown, usize::MAX);
        } else {
            hold.budget.release(grown);
        }
        Some(delta)
    }
}

/// Reads `body` until its model is known (or it ends), holding what it read.
///
/// Cancel safety: dropping the future drops the held chunks and releases
/// their budget.
pub(crate) async fn hold_until_model<B>(
    body: &mut B,
    budget: &Gauge,
    max_held: usize,
    max_body: usize,
    token_limit: Option<u64>,
) -> Result<HeldStart, HoldError>
where
    B: Body<Data = Bytes> + Unpin,
{
    let mut hold = Box::new(Hold {
        chunks: Chunks::default(),
        budget: budget.hold_none(),
        scanner: PolicyScanner::new(),
        ended: false,
        token_limit,
        check_rest: true,
    });
    let mut total = 0usize;
    while hold.scanner.model().is_none() {
        let Some(frame) = body.frame().await else {
            hold.ended = true;
            hold.scanner.finish().map_err(HoldError::Model)?;
            break;
        };
        // Request trailers carry nothing a provider reads; only data counts.
        let Ok(data) = frame.map_err(|_| HoldError::Body)?.into_data() else {
            continue;
        };
        total = total.saturating_add(data.len());
        if total > max_body {
            return Err(HoldError::TooLarge);
        }
        if !hold.budget.try_grow(data.len(), max_held) {
            return Err(HoldError::Overloaded);
        }
        hold.scanner.feed(&data);
        hold.chunks.push(data);
        if let Some(error) = hold.scanner.error() {
            return Err(HoldError::Model(error));
        }
    }
    Ok(HeldStart(hold))
}

/// The request body sent upstream: held chunks first, then the rest of the
/// client's body, checked for a repeated `model` when permissions apply.
pub(crate) struct RequestBody<B> {
    /// `None` once the client's body has ended or failed the check.
    inner: Option<B>,
    hold: Option<Box<Hold>>,
}

impl<B> RequestBody<B> {
    /// The client's body unchanged: no permissions apply.
    pub(crate) fn new(inner: B) -> Self {
        Self {
            inner: Some(inner),
            hold: None,
        }
    }

    /// `held` followed by the rest of `inner`.
    pub(crate) fn after_hold(held: HeldStart, inner: B) -> Self {
        Self {
            inner: (!held.0.ended).then_some(inner),
            hold: Some(held.0),
        }
    }
}

impl<B> Body for RequestBody<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if let Some(hold) = this.hold.as_mut()
            && let Some(chunk) = hold.chunks.pop()
        {
            hold.budget.release(chunk.len());
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        let polled = Pin::new(inner).poll_frame(cx);
        if let (Poll::Ready(Some(Ok(frame))), Some(hold)) = (&polled, this.hold.as_mut())
            && hold.check_rest
            && let Some(data) = frame.data_ref()
        {
            hold.scanner.feed(data);
            // Either failure stops the upstream request mid-body: the provider
            // never sees a complete request that breaks the policy.
            if let Some(error) = hold.scanner.error() {
                this.inner = None;
                return Poll::Ready(Some(Err(Box::new(error))));
            }
            if let (Some(limit), Some(requested)) = (hold.token_limit, hold.scanner.max_tokens())
                && requested > limit
            {
                this.inner = None;
                return Poll::Ready(Some(Err(Box::new(OverTokenLimit { requested, limit }))));
            }
        }
        polled.map(|frame| frame.map(|f| f.map_err(Into::into)))
    }

    fn is_end_stream(&self) -> bool {
        self.hold.as_ref().is_none_or(|h| h.chunks.is_empty())
            && self.inner.as_ref().is_none_or(B::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        let held = self.hold.as_ref().map_or(0, |h| h.chunks.bytes());
        let rest = self
            .inner
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), B::size_hint);
        let mut hint = SizeHint::new();
        hint.set_lower(rest.lower().saturating_add(held));
        if let Some(upper) = rest.upper() {
            hint.set_upper(upper.saturating_add(held));
        }
        hint
    }
}

#[cfg(test)]
mod tests;
