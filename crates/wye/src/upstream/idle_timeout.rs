//! Upstream response body that fails when the upstream goes silent.

use std::{
    error::Error as StdError,
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use tokio::time::{Instant, Sleep};

pub(crate) type BoxError = Box<dyn StdError + Send + Sync>;

/// Fails the stream when the upstream sends nothing for `timeout`.
///
/// The timer runs only while this body waits on the upstream. Time spent
/// while the downstream is not polling (backpressure) never counts.
pub(crate) struct IdleTimeout<B> {
    inner: B,
    timeout: Duration,
    sleep: Pin<Box<Sleep>>,
    waiting: bool,
}

impl<B> IdleTimeout<B> {
    pub(super) fn new(inner: B, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            sleep: Box::pin(tokio::time::sleep(timeout)),
            waiting: false,
        }
    }
}

#[derive(Debug)]
struct StreamIdleTimeout;

impl fmt::Display for StreamIdleTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("upstream stream idle timeout")
    }
}

impl StdError for StreamIdleTimeout {}

impl<B> Body for IdleTimeout<B>
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
                this.waiting = false;
                Poll::Ready(frame.map(|f| f.map_err(Into::into)))
            }
            Poll::Pending => {
                if !this.waiting {
                    this.waiting = true;
                    this.sleep.as_mut().reset(Instant::now() + this.timeout);
                }
                match this.sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Some(Err(StreamIdleTimeout.into()))),
                    Poll::Pending => Poll::Pending,
                }
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
