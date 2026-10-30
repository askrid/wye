//! The request body as forwarded, held until its `model` is read, so model
//! permissions and the output-token ceiling (the API's output-token field,
//! when it arrives in time) are checked before anything is sent upstream.
//!
//! [`hold_until_model`] reads the request body only until its top-level
//! `model` value has been read. Claude Code sends `model` first, so usually
//! only the first chunk waits. The held bytes count against a process-wide
//! budget (`server.max_held_request_bytes`). When the model is allowed,
//! [`RequestBody`] replays the held chunks and streams the rest, and keeps
//! checking it: a second top-level `model` key, an output-token maximum above
//! the ceiling that arrives after the hold, or (on the OpenAI APIs, where the
//! field is optional) none at all, fails the body, which aborts the upstream
//! request before the provider sees a complete request.
//!
//! The same hold resolves model aliases (spliced in place) and finds where
//! `model` is, so each attempt can send the provider's own model id
//! (Bedrock's `anthropic.` prefix, [`chunks`]).

use std::{
    ops::Range,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use gateway_core::{
    admission::TokenLimitError,
    api::Api,
    policy::{PolicyError, PolicyScanner},
};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::body::{Body, Frame, Incoming, SizeHint};

use self::chunks::Chunks;
pub(crate) use self::chunks::splice_chunks;
use crate::{
    BoxError,
    gauge::{Gauge, Held},
};

mod chunks;

/// The client's body, bounded by `server.max_request_body_bytes`: the limit
/// counts what the client sent, not a `model` spliced in.
pub(crate) type ClientBody = Limited<Incoming>;

/// The least a held chunk counts against the hold and failover budgets.
/// Beyond its bytes, a chunk costs its handle, its queue slot, and the
/// framing it pins in the client connection's read buffer: a body sent in
/// tiny chunks (1-byte HTTP/1.1 chunks, small HTTP/2 frames) would hold tens
/// of bytes of memory per byte counted. Counting a small chunk as 1 KiB keeps
/// each budget an upper bound on memory; ordinary chunks count their length.
pub(crate) const MIN_CHUNK_CHARGE: usize = 1024;

/// How long a held body may send nothing before its request is refused
/// (408). A client that stops mid-upload, or vanished without closing its
/// connection, would otherwise keep its permit and its share of the hold
/// budget for good: nothing else reads a request body against a clock, and
/// a connection to a peer that is gone stays open while Wye only reads.
pub(crate) const HOLD_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// What a held chunk of `len` bytes counts against a budget.
pub(crate) fn charge(len: usize) -> usize {
    len.max(MIN_CHUNK_CHARGE)
}

/// What `chunks` count against a budget together.
pub(crate) fn charged<'c>(chunks: impl IntoIterator<Item = &'c Bytes>) -> usize {
    chunks.into_iter().map(|c| charge(c.len())).sum()
}

/// Moves `budget` from `before` to `after` charged bytes once a splice
/// changed the held chunks. At most a model name's length and two new chunks
/// per request, so the limit is not re-checked.
pub(crate) fn recharge(budget: &mut Held, before: usize, after: usize) {
    if after > before {
        budget.try_grow(after - before, usize::MAX);
    } else {
        budget.release(before - after);
    }
}

/// The size of `prefix` bytes followed by `rest` (`None`: ended).
pub(crate) fn hint_after<B: Body>(prefix: u64, rest: Option<&B>) -> SizeHint {
    let rest = rest.map_or_else(|| SizeHint::with_exact(0), B::size_hint);
    let mut hint = SizeHint::new();
    hint.set_lower(rest.lower().saturating_add(prefix));
    if let Some(upper) = rest.upper() {
        hint.set_upper(upper.saturating_add(prefix));
    }
    hint
}

/// Why a request was refused while its model was read.
#[derive(Debug)]
pub(crate) enum HoldError {
    /// The body's `model` cannot be checked.
    Model(PolicyError),
    /// The process-wide hold budget is used up.
    Overloaded,
    /// The body exceeded the request size limit while being held.
    TooLarge,
    /// The client sent nothing for [`HOLD_IDLE_TIMEOUT`].
    Stalled,
    /// The client's body failed (for example, the connection dropped).
    Body,
}

/// What a held request carries until its body is fully sent.
struct Hold {
    chunks: Chunks,
    /// The [`charge`] of each of `chunks`, counted against the hold budget.
    budget: Held,
    scanner: PolicyScanner,
    /// The whole body is in `chunks`.
    ended: bool,
    /// The request's output-token ceiling, checked again as the rest streams.
    token_limit: Option<u64>,
    /// The API: which field holds the output-token maximum, and whether a
    /// ceiling requires it.
    api: Api,
    /// Whether the rest of the body is checked as it streams (a repeated
    /// `model`, a late output-token field); off when no permission or ceiling
    /// applies.
    check_rest: bool,
}

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

    /// Whether the hold read the whole body.
    pub(crate) fn ended(&self) -> bool {
        self.0.ended
    }

    /// The request's output-token maximum, if it was read before the hold ended.
    pub(crate) fn max_output_tokens(&self) -> Option<u64> {
        self.0.scanner.max_output_tokens()
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
    pub(crate) fn splice_model(&mut self, model: Bytes) -> Option<i64> {
        let span = self.0.scanner.model_span()?;
        Some(self.0.splice(&span, model))
    }

    /// Where the client's `model` value is in the body as received.
    pub(crate) fn model_span(&self) -> Option<Range<u64>> {
        self.0.scanner.model_span()
    }
}

impl Hold {
    /// Replaces the held bytes at `span` with `value`; returns the change in
    /// length. The hold covers every byte of `span`: it stopped right after
    /// the model's value.
    fn splice(&mut self, span: &Range<u64>, value: Bytes) -> i64 {
        let delta = value.len() as i64 - (span.end - span.start) as i64;
        let before = self.chunks.charged();
        let mut out = Chunks::default();
        splice_chunks(std::iter::from_fn(|| self.chunks.pop()), span, value, |c| {
            out.push(c);
        });
        self.chunks = out;
        recharge(&mut self.budget, before, self.chunks.charged());
        delta
    }
}

/// Reads `body` until its model is known (or it ends), holding what it read.
///
/// Cancel safety: dropping the future drops the held chunks and releases
/// their budget.
pub(crate) async fn hold_until_model<B>(
    body: &mut B,
    api: Api,
    budget: &Gauge,
    max_held: usize,
    token_limit: Option<u64>,
) -> Result<HeldStart, HoldError>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    let mut hold = Box::new(Hold {
        chunks: Chunks::default(),
        budget: budget.hold_none(),
        scanner: PolicyScanner::for_api(api),
        ended: false,
        token_limit,
        api,
        check_rest: true,
    });
    while hold.scanner.model().is_none() {
        let frame = tokio::time::timeout(HOLD_IDLE_TIMEOUT, body.frame())
            .await
            .map_err(|_| HoldError::Stalled)?;
        let Some(frame) = frame else {
            hold.ended = true;
            hold.scanner.finish().map_err(HoldError::Model)?;
            break;
        };
        // Request trailers carry nothing a provider reads; only data counts.
        let Ok(data) = frame.map_err(held_body_error)?.into_data() else {
            continue;
        };
        if !hold.budget.try_grow(charge(data.len()), max_held) {
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

/// The client's body failed while held: past the size limit ([`ClientBody`]),
/// or cut off.
fn held_body_error(error: impl Into<BoxError>) -> HoldError {
    if error.into().is::<LengthLimitError>() {
        HoldError::TooLarge
    } else {
        HoldError::Body
    }
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

    /// Replaces the bytes at `span` (the model's value) with `value` before
    /// anything was sent; `None` when the body was not held, so the model's
    /// position is unknown. Returns the change in length.
    pub(crate) fn splice(&mut self, span: &Range<u64>, value: Bytes) -> Option<i64> {
        Some(self.hold.as_mut()?.splice(span, value))
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
            hold.budget.release(charge(chunk.len()));
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        let polled = Pin::new(inner).poll_frame(cx);
        if let (Poll::Ready(Some(Ok(frame))), Some(hold)) = (&polled, this.hold.as_mut())
            && hold.check_rest
        {
            if let Some(data) = frame.data_ref() {
                hold.scanner.feed(data);
            }
            // Either failure stops the upstream request mid-body: the provider
            // never sees a complete request that breaks the policy.
            if let Some(error) = hold.scanner.error() {
                this.inner = None;
                return Poll::Ready(Some(Err(Box::new(error))));
            }
            if let Some(limit) = hold.token_limit {
                // The field may still come later: only a value is final
                // before the body ends. Trailers end it: hyper polls no
                // further.
                let ended = frame.is_trailers() || this.inner.as_ref().is_none_or(B::is_end_stream);
                let requested = hold.scanner.max_output_tokens();
                if (requested.is_some() || ended)
                    && let Some(refusal) = TokenLimitError::check(hold.api, limit, requested)
                {
                    this.inner = None;
                    return Poll::Ready(Some(Err(Box::new(refusal))));
                }
            }
        }
        // A body without a known length ends with `Ready(None)`: a missing
        // required field is refused instead of ending the request.
        if let (Poll::Ready(None), Some(hold)) = (&polled, this.hold.as_ref())
            && hold.check_rest
            && let Some(limit) = hold.token_limit
            && let Some(refusal) =
                TokenLimitError::check(hold.api, limit, hold.scanner.max_output_tokens())
        {
            this.inner = None;
            return Poll::Ready(Some(Err(Box::new(refusal))));
        }
        polled.map(|frame| frame.map(|f| f.map_err(Into::into)))
    }

    fn is_end_stream(&self) -> bool {
        self.hold.as_ref().is_none_or(|h| h.chunks.is_empty())
            && self.inner.as_ref().is_none_or(B::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        let held = self.hold.as_ref().map_or(0, |h| h.chunks.bytes());
        hint_after(held, self.inner.as_ref())
    }
}

#[cfg(test)]
mod tests;
