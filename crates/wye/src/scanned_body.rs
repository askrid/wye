//! A request body that feeds the bytes hyper sends upstream to a scanner,
//! without changing or delaying them, and stores the scanner's result in a
//! [`Slot`] when the body ends: prompt hashes ([`crate::diagnostics`]) and
//! upgrade-check findings ([`crate::upgrade_check`]).

use std::{
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};

/// A scan result, set once the whole body was read. Shared between the
/// request body (in hyper's client) and the usage record; an error or a
/// dropped body leaves it empty.
pub(crate) type Slot<T> = Arc<OnceLock<T>>;

/// What [`ScannedBody`] feeds.
pub(crate) trait BodyScanner: Default {
    type Output;

    fn feed(&mut self, chunk: &[u8]);

    /// `None` when the scanner could not follow the body.
    fn finish(self) -> Option<Self::Output>;
}

/// `inner`, scanned by `S` when a slot is given.
pub(crate) struct ScannedBody<B, S: BodyScanner> {
    inner: B,
    /// Boxed so the wrapper stays small when the scan is off.
    scan: Option<(Box<S>, Slot<S::Output>)>,
}

impl<B, S: BodyScanner> ScannedBody<B, S> {
    pub(crate) fn new(inner: B, slot: Option<Slot<S::Output>>) -> Self {
        Self {
            inner,
            scan: slot.map(|slot| (Box::default(), slot)),
        }
    }

    fn finish(&mut self) {
        if let Some((scanner, slot)) = self.scan.take()
            && let Some(output) = (*scanner).finish()
        {
            let _ = slot.set(output);
        }
    }
}

impl<B, S> Body for ScannedBody<B, S>
where
    B: Body<Data = Bytes> + Unpin,
    S: BodyScanner,
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
