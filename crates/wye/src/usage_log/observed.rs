//! The response body wrapper that observes usage and emits one record.

use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use gateway_core::usage::SseUsageScanner;
use http::StatusCode;
use hyper::body::{Body, Frame, SizeHint};

use super::{Outcome, Record, RequestInfo, UsageLog, unix_ms};
use crate::gauge::Held;

/// Sent when a successful event stream fails at an event boundary (an idle
/// timeout or a broken upstream body), so the client sees a retryable API
/// error instead of a dropped connection. Anthropic's streams report
/// mid-stream overload the same way.
const STALLED_EVENT: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"upstream stream stalled or broke; retry the request\"}}\n\n";

/// Response body wrapper that observes usage and emits a record on drop.
pub(crate) struct Observed<B> {
    inner: B,
    /// Present for successful SSE responses only.
    scanner: Option<SseUsageScanner>,
    /// The bytes forwarded so far end at an SSE event boundary.
    at_boundary: bool,
    /// The stream was closed with [`STALLED_EVENT`].
    closed: bool,
    info: Option<RequestInfo>,
    status: StatusCode,
    headers_after: Duration,
    outcome: Outcome,
    log: UsageLog,
    /// The request's concurrency permit, released when the response body is dropped.
    _permit: Held,
}

impl<B> Observed<B> {
    pub(crate) fn new(
        inner: B,
        info: RequestInfo,
        status: StatusCode,
        is_event_stream: bool,
        log: UsageLog,
        permit: Held,
    ) -> Self {
        let headers_after = info.started.elapsed();
        Self {
            inner,
            scanner: (status.is_success() && is_event_stream).then(SseUsageScanner::new),
            at_boundary: true,
            closed: false,
            info: Some(info),
            status,
            headers_after,
            outcome: Outcome::Cancelled,
            log,
            _permit: permit,
        }
    }
}

impl<B> Body for Observed<B>
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
        if this.closed {
            return Poll::Ready(None);
        }
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(scanner), Some(data)) = (this.scanner.as_mut(), frame.data_ref()) {
                    scanner.feed(data);
                    if !data.is_empty() {
                        this.at_boundary = data.ends_with(b"\n\n") || data.ends_with(b"\r\n\r\n");
                    }
                }
                // With a known length, hyper stops polling once the body reports its end
                // and never sees `Ready(None)`.
                if this.inner.is_end_stream() {
                    this.outcome = Outcome::Completed;
                }
            }
            Poll::Ready(Some(Err(_))) => {
                this.outcome = Outcome::Failed;
                if this.scanner.is_some() && this.at_boundary {
                    this.closed = true;
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(STALLED_EVENT)))));
                }
            }
            Poll::Ready(None) => this.outcome = Outcome::Completed,
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.closed || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for Observed<B> {
    fn drop(&mut self) {
        let Some(info) = self.info.take() else { return };
        let (model, usage) = match self.scanner.take() {
            Some(scanner) => (
                scanner.model().map(Box::from),
                Some((scanner.usage(), scanner.status().as_str())),
            ),
            None => (None, None),
        };
        let duration = info.started.elapsed();
        self.log.send(Record {
            info,
            status: self.status,
            headers_after: Some(self.headers_after),
            duration,
            outcome: self.outcome,
            model,
            usage,
            unix_ms: unix_ms(),
        });
    }
}
