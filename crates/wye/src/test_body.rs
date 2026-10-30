//! A client's request body for unit tests.

use std::{
    collections::VecDeque,
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use hyper::body::{Body, Frame};

/// A client body that yields its chunks, one per poll.
pub(crate) struct Client(pub(crate) VecDeque<Bytes>);

impl Client {
    /// The body of `parts`, one chunk each.
    pub(crate) fn of(parts: &[&'static str]) -> Self {
        Self(
            parts
                .iter()
                .map(|p| Bytes::from_static(p.as_bytes()))
                .collect(),
        )
    }
}

/// A client that sends its chunks, then nothing more, without closing.
pub(crate) struct Stalling(pub(crate) Client);

impl Body for Stalling {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match Pin::new(&mut self.0).poll_frame(cx) {
            Poll::Ready(None) => Poll::Pending,
            sent => sent,
        }
    }
}

impl Body for Client {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.0.pop_front().map(|c| Ok(Frame::data(c))))
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_empty()
    }
}
