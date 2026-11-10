//! A request body that feeds the bytes hyper sends upstream to a scanner,
//! without changing or delaying them, and stores the scanner's result in a
//! [`Slot`] when the body ends: prompt hashes for cache diagnostics and
//! upgrade-check findings, which the usage record reads when the response
//! ends.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use gateway_core::{
    prompt::{PromptHashes, PromptScanner},
    upgrade::{Findings, UpgradeScanner},
};
use hyper::body::{Body, Frame, SizeHint};

use crate::Slot;

/// Prompt hashes of one request body, for cache diagnostics.
pub(crate) type PromptSlot = Slot<PromptHashes>;

/// Upgrade-check findings of one request body.
pub(crate) type UpgradeSlot = Slot<Findings>;

/// What [`ScannedBody`] feeds.
pub(crate) trait BodyScanner: Default {
    type Output;

    fn feed(&mut self, chunk: &[u8]);

    /// `None` when the scanner could not follow the body.
    fn finish(self) -> Option<Self::Output>;
}

impl BodyScanner for PromptScanner {
    type Output = PromptHashes;

    fn feed(&mut self, chunk: &[u8]) {
        PromptScanner::feed(self, chunk);
    }

    fn finish(self) -> Option<PromptHashes> {
        PromptScanner::finish(self)
    }
}

impl BodyScanner for UpgradeScanner {
    type Output = Findings;

    fn feed(&mut self, chunk: &[u8]) {
        UpgradeScanner::feed(self, chunk);
    }

    fn finish(self) -> Option<Findings> {
        Some(self.findings())
    }
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
