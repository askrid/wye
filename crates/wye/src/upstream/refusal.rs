//! Why Wye's own request body stopped an upstream request: a policy check
//! on the streamed part of the body, or the size limit.
//!
//! hyper's HTTP/1.1 client keeps a body's error as the source of the request
//! error. Its HTTP/2 client only logs the error and resets the stream, so on
//! an upstream that may speak HTTP/2 the body also leaves the refusal in a
//! [`RefusalSlot`].

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use gateway_core::policy::PolicyError;
use http_body_util::LengthLimitError;
use hyper::body::{Body, Frame, SizeHint};

use super::BoxError;
use crate::{request_body::TokenLimitError, scanned_body::Slot};

/// A refusal raised after forwarding began; the upstream request was aborted.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PolicyRefusal {
    /// A repeated `model`, an escaped top-level key, or a bad output-token field.
    Invalid(PolicyError),
    /// The output-token maximum above the request's ceiling, or missing.
    TokenLimit(TokenLimitError),
    /// The body grew past `server.max_request_body_bytes`.
    TooLarge,
}

/// The refusal an attempt's request body raised.
pub(super) type RefusalSlot = Slot<PolicyRefusal>;

/// The refusal among `error` and its sources, if Wye's request body raised it.
pub(super) fn policy_refusal(error: &(dyn std::error::Error + 'static)) -> Option<PolicyRefusal> {
    let mut cause = Some(error);
    while let Some(e) = cause {
        if let Some(e) = e.downcast_ref::<PolicyError>() {
            return Some(PolicyRefusal::Invalid(*e));
        }
        if let Some(e) = e.downcast_ref::<TokenLimitError>() {
            return Some(PolicyRefusal::TokenLimit(*e));
        }
        if e.is::<LengthLimitError>() {
            return Some(PolicyRefusal::TooLarge);
        }
        cause = e.source();
    }
    None
}

/// `inner`, leaving the refusal its error carries in a slot when given.
pub(crate) struct Refusing<B> {
    inner: B,
    slot: Option<RefusalSlot>,
}

impl<B> Refusing<B> {
    pub(super) fn new(inner: B, slot: Option<RefusalSlot>) -> Self {
        Self { inner, slot }
    }
}

impl<B> Body for Refusing<B>
where
    B: Body<Data = Bytes, Error = BoxError> + Unpin,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        if let (Poll::Ready(Some(Err(e))), Some(slot)) = (&polled, &this.slot)
            && let Some(refusal) = policy_refusal(&**e)
        {
            // Set before hyper resets the stream, so the request error that
            // follows finds it.
            let _ = slot.set(refusal);
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
