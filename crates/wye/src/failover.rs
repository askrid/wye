//! Resending a request to the route's next upstream after a rejection that
//! proves the request was not processed: `429`, `529`, a connection refused,
//! or one cut before the body was delivered whole ([`Resend::sent_whole`]).
//!
//! hyper consumes the request body and drops it, so resending needs the
//! bytes back. [`FailoverBody`] keeps every chunk it sends (shared `Bytes`,
//! not copies) within a per-request and a process-wide bound, and when it is
//! dropped hands back what it sent plus the part of the client's body not
//! read yet ([`Resend`]), through a oneshot channel: no lock, no shared
//! state. The next attempt replays the sent chunks and then reads on, so a
//! rejection that arrives before the upload finished can still move on.
//! Beyond either bound the body stops keeping, hands nothing back, and the
//! request is forwarded without failover, never refused.

use std::{
    ops::Range,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use tokio::sync::oneshot;

use crate::{
    gauge::{Gauge, Reserved},
    request_body::{RequestBody, charge, charged, hint_after, recharge, splice_chunks},
};

/// What a dropped attempt hands back: the bytes it sent, in order, then the
/// client's body from where it stopped reading.
pub(crate) struct Resend<B> {
    chunks: Vec<Bytes>,
    rest: Option<B>,
    /// The [`charge`] of the kept chunks, counted against the process-wide
    /// bound.
    budget: Reserved,
    bytes: usize,
    /// The attempt delivered every byte: nothing was left to replay and the
    /// client's body was read to its end.
    whole: bool,
}

impl<B> Resend<B> {
    /// Whether the attempt delivered the whole body. When it did not, the
    /// provider cannot have received a complete request.
    pub(crate) fn sent_whole(&self) -> bool {
        self.whole
    }
}

impl<B> Resend<RequestBody<B>> {
    /// Replaces the bytes at `span` (the model's value, in body offsets)
    /// with `value`, wherever they are: in what was sent, or still in the
    /// unread rest. `false` when the value straddles the two, so the body
    /// cannot be re-prefixed.
    pub(crate) fn splice(&mut self, span: &Range<u64>, value: Bytes) -> bool {
        let sent: u64 = self.chunks.iter().map(|c| c.len() as u64).sum();
        let delta = value.len() as i64 - (span.end - span.start) as i64;
        if span.end <= sent {
            let before = charged(&self.chunks);
            let mut out = Vec::with_capacity(self.chunks.len() + 2);
            splice_chunks(self.chunks.drain(..), span, value, |c| out.push(c));
            self.chunks = out;
            recharge(&mut self.budget, before, charged(&self.chunks));
            self.bytes = self.bytes.saturating_add_signed(delta as isize);
            true
        } else if span.start >= sent {
            // The rest's held chunks start where the sent ones end.
            let shifted = span.start - sent..span.end - sent;
            self.rest
                .as_mut()
                .is_some_and(|rest| rest.splice(&shifted, value))
        } else {
            false
        }
    }
}

/// The bounds a kept body stays within.
#[derive(Clone, Copy)]
pub(crate) struct Bounds {
    /// Request bytes kept for one request.
    pub(crate) max_body: usize,
    /// Request bytes kept across the process ([`Gauge`]).
    pub(crate) max_kept: usize,
}

/// Boxed, and only on routes with failover: hyper keeps request bodies
/// inline in its per-connection queue, so the body type stays small.
struct Keep<B> {
    /// Every chunk kept, in body order: an earlier attempt's, sent again
    /// first, then the ones read from the client's body.
    chunks: Vec<Bytes>,
    /// The chunks before it were sent by this attempt.
    next: usize,
    budget: Reserved,
    bytes: usize,
    /// The client's body was read to its end in this attempt.
    ended: bool,
    bounds: Bounds,
    back: oneshot::Sender<Resend<B>>,
}

/// A request body that can be handed back for another attempt.
pub(crate) struct FailoverBody<B> {
    /// The client's body, until it ended.
    rest: Option<B>,
    /// Present while the body keeps what it sends. It stops keeping only
    /// while reading `rest`, after any replay was sent.
    keep: Option<Box<Keep<B>>>,
}

/// Receives what an attempt hands back when it is dropped; closed without
/// a value when the attempt stopped keeping.
pub(crate) type Handback<B> = oneshot::Receiver<Resend<B>>;

impl<B> FailoverBody<B> {
    /// The client's body, sent once: no failover.
    pub(crate) fn once(inner: B) -> Self {
        Self {
            rest: Some(inner),
            keep: None,
        }
    }

    /// The client's body, kept within `bounds` for another attempt.
    pub(crate) fn kept(inner: B, kept: &Gauge, bounds: Bounds) -> (Self, Handback<B>) {
        Self::resume(
            Resend {
                chunks: Vec::new(),
                rest: Some(inner),
                budget: kept.reserve_none(),
                bytes: 0,
                whole: false,
            },
            bounds,
        )
    }

    /// The body an earlier attempt handed back, kept again for the next.
    pub(crate) fn resume(resend: Resend<B>, bounds: Bounds) -> (Self, Handback<B>) {
        let (back, handback) = oneshot::channel();
        (
            Self {
                rest: resend.rest,
                keep: Some(Box::new(Keep {
                    chunks: resend.chunks,
                    next: 0,
                    budget: resend.budget,
                    bytes: resend.bytes,
                    ended: false,
                    bounds,
                    back,
                })),
            },
            handback,
        )
    }
}

impl<B> Body for FailoverBody<B>
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
        if let Some(keep) = this.keep.as_mut()
            && let Some(chunk) = keep.chunks.get(keep.next).cloned()
        {
            // Already counted when first sent.
            keep.next += 1;
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        let Some(rest) = this.rest.as_mut() else {
            return Poll::Ready(None);
        };
        let polled = Pin::new(&mut *rest).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(data), Some(keep)) = (frame.data_ref(), this.keep.as_mut()) {
                    let fits = keep.bytes + data.len() <= keep.bounds.max_body
                        && keep
                            .budget
                            .try_grow(charge(data.len()), keep.bounds.max_kept);
                    if fits {
                        keep.bytes += data.len();
                        keep.chunks.push(data.clone());
                        keep.next = keep.chunks.len();
                        keep.ended = rest.is_end_stream();
                    } else {
                        this.keep = None;
                    }
                } else if let Some(keep) = this.keep.as_mut()
                    && frame.is_trailers()
                {
                    // The last frame: hyper polls no further. A resend leaves
                    // the trailers out; providers read none.
                    keep.ended = true;
                }
            }
            Poll::Ready(None) => this.rest = None,
            // The client's body failed: nothing to resend.
            Poll::Ready(Some(Err(_))) => this.keep = None,
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.keep.as_ref().is_none_or(|k| k.next == k.chunks.len())
            && self.rest.as_ref().is_none_or(B::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        let replay: u64 = self
            .keep
            .iter()
            .flat_map(|k| k.chunks.get(k.next..).unwrap_or_default())
            .map(|c| c.len() as u64)
            .sum();
        hint_after(replay, self.rest.as_ref())
    }
}

impl<B> Drop for FailoverBody<B> {
    fn drop(&mut self) {
        let Some(keep) = self.keep.take() else {
            return;
        };
        let Keep {
            chunks,
            next,
            budget,
            bytes,
            ended,
            back,
            ..
        } = *keep;
        let whole = next == chunks.len() && (ended || self.rest.is_none());
        // A receiver gone means the request is answered; the bytes are dropped.
        let _ = back.send(Resend {
            chunks,
            rest: self.rest.take(),
            budget,
            bytes,
            whole,
        });
    }
}

#[cfg(test)]
mod tests;
