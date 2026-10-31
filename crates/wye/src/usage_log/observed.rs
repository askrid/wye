//! The response body wrapper that observes usage and emits one record.

use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use gateway_core::usage::UsageScanner;
use http::{HeaderValue, StatusCode};
use hyper::body::{Body, Frame, SizeHint};

use super::{Outcome, RequestInfo, Seen, UsageLog};
use crate::gauge::Held;

/// Sent when a successful Messages event stream fails at an event boundary
/// (an idle timeout or a broken upstream body), so the client sees a
/// retryable API error instead of a dropped connection. Anthropic's streams
/// report mid-stream overload the same way. A Responses stream just ends:
/// Codex and the OpenAI SDKs retry a stream that ends before its terminal
/// event.
const STALLED_EVENT: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"upstream stream stalled or broke; retry the request\"}}\n\n";

/// Response body wrapper that observes usage and emits a record on drop.
pub(crate) struct Observed<B> {
    inner: B,
    /// Present for successful responses of endpoints that generate: SSE
    /// streams and JSON bodies.
    scanner: Option<UsageScanner>,
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

impl<B: Body> Observed<B> {
    pub(crate) fn new(
        inner: B,
        info: RequestInfo,
        status: StatusCode,
        content_type: Option<&HeaderValue>,
        log: UsageLog,
        permit: Held,
    ) -> Self {
        let headers_after = info.started.elapsed();
        let api = info.endpoint.api;
        // count_tokens answers with a count, not usage.
        let scanner = match content_type.map(HeaderValue::as_bytes) {
            _ if !status.is_success() || !info.endpoint.generates => None,
            Some(t) if t.starts_with(b"text/event-stream") => Some(UsageScanner::new(api)),
            Some(t) if t.starts_with(b"application/json") => Some(UsageScanner::json(api)),
            _ => None,
        };
        // hyper never polls a body that is already at its end (a 204, a
        // `content-length: 0`): it is delivered with the headers.
        let outcome = if inner.is_end_stream() {
            Outcome::Completed
        } else {
            Outcome::Cancelled
        };
        Self {
            inner,
            scanner,
            at_boundary: true,
            closed: false,
            info: Some(info),
            status,
            headers_after,
            outcome,
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
                // and never sees `Ready(None)`; and the client connection stops once the
                // declared length is written, while an HTTP/2 upstream may still have its
                // end-of-stream frame to send.
                if this.inner.is_end_stream() || this.inner.size_hint().exact() == Some(0) {
                    this.outcome = Outcome::Completed;
                }
            }
            Poll::Ready(Some(Err(_))) => {
                this.outcome = Outcome::Failed;
                if matches!(this.scanner, Some(UsageScanner::Messages(_))) && this.at_boundary {
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
        let mut seen = Seen {
            headers_after: Some(self.headers_after),
            ..Seen::default()
        };
        if let Some(mut scanner) = self.scanner.take() {
            seen.model = scanner.model().map(Box::from);
            seen.usage = Some((scanner.usage(), scanner.status()));
            seen.tool_calls = scanner.take_tool_calls();
        }
        self.log.record(info, self.status, self.outcome, seen);
    }
}
