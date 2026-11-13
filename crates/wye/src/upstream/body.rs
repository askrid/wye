//! Upstream response body: holds the connection slot and fails when the
//! upstream goes silent.

use std::{
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};

use super::error::IdleTimeout;
use crate::{BoxError, gauge::Reserved, idle::IdleTimer};

/// Fails the stream when the upstream sends nothing for `timeout`. The timer
/// runs only while this body waits on the upstream: time the downstream is not
/// polling (backpressure) never counts.
pub(crate) struct UpstreamBody<B> {
    inner: B,
    idle: IdleTimer,
    _slot: Reserved,
}

impl<B> UpstreamBody<B> {
    pub(super) fn new(inner: B, timeout: Duration, slot: Reserved) -> Self {
        Self {
            inner,
            idle: IdleTimer::new(timeout),
            _slot: slot,
        }
    }
}

impl<B> Body for UpstreamBody<B>
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
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(frame) => {
                this.idle.progressed();
                Poll::Ready(frame.map(|f| f.map_err(Into::into)))
            }
            Poll::Pending => {
                ready!(this.idle.poll_expired(cx));
                Poll::Ready(Some(Err(Box::new(IdleTimeout))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
