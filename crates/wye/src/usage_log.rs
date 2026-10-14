//! Per-request usage records, written as JSON lines off the request path.
//!
//! The response body is wrapped in [`Observed`], which feeds the forwarded
//! bytes to a [`SseUsageScanner`] without changing or delaying them and emits
//! one [`Record`] when the body is dropped: after completion, error, or client
//! disconnect. Records go through a bounded channel to one writer task; when the
//! channel is full the record is dropped and counted, so a slow log sink never
//! slows a stream. This is the interim sink until OTLP export (M1 stage 5).

use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use gateway_core::{
    credential::Identity,
    prompt::HashKey,
    usage::{SseUsageScanner, Usage},
};

use crate::{
    diagnostics::{PromptRecord, PromptSlot},
    gauge::Held,
};
use http::{HeaderValue, StatusCode};
use hyper::body::{Body, Frame, SizeHint};
use serde::Serialize;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};

/// Records buffered before new ones are dropped.
const QUEUE_CAPACITY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The upstream body ended normally.
    Completed,
    /// The upstream body failed (for example, the idle timeout) after headers were sent.
    Failed,
    /// The client went away before the upstream body ended.
    Cancelled,
    /// Wye answered without a usable upstream response.
    GatewayError,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::GatewayError => "gateway_error",
        }
    }
}

/// Request facts known before the response arrives.
pub(crate) struct RequestInfo {
    pub path: &'static str,
    pub session_id: Option<HeaderValue>,
    /// Verified gateway credential, when credentials are configured.
    pub identity: Option<Arc<Identity>>,
    /// Filled with the request body's prompt hashes when cache diagnostics are on.
    pub prompt: Option<PromptSlot>,
    pub started: Instant,
}

pub(crate) struct Record {
    info: RequestInfo,
    status: StatusCode,
    headers_after: Option<Duration>,
    duration: Duration,
    outcome: Outcome,
    model: Option<Box<str>>,
    usage: Option<(Usage, &'static str)>,
    unix_ms: u128,
}

/// Cloneable handle that queues records for the writer task.
#[derive(Clone)]
pub(crate) struct UsageLog {
    tx: mpsc::Sender<Record>,
    dropped: Arc<AtomicU64>,
}

impl UsageLog {
    /// Starts the writer task. It ends after every handle is dropped and the queue is written.
    /// `prompt_key` keys the prompt hashes of records that carry them.
    pub(crate) fn start<W>(out: W, prompt_key: Option<Arc<HashKey>>) -> (Self, JoinHandle<()>)
    where
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer = tokio::spawn(write_records(rx, out, Arc::clone(&dropped), prompt_key));
        (Self { tx, dropped }, writer)
    }

    fn send(&self, record: Record) {
        if self.tx.try_send(record).is_err() {
            // Statistics counter only; no ordering with other memory is needed.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records a response Wye generated itself.
    pub(crate) fn gateway_error(&self, info: RequestInfo, status: StatusCode) {
        let duration = info.started.elapsed();
        self.send(Record {
            info,
            status,
            headers_after: None,
            duration,
            outcome: Outcome::GatewayError,
            model: None,
            usage: None,
            unix_ms: unix_ms(),
        });
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

async fn write_records<W>(
    mut rx: mpsc::Receiver<Record>,
    mut out: W,
    dropped: Arc<AtomicU64>,
    prompt_key: Option<Arc<HashKey>>,
) where
    W: AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(4096);
    let key = prompt_key.as_deref();
    while let Some(record) = rx.recv().await {
        push_line(&mut buf, &record, key);
        // Batch whatever else is queued into one write.
        while let Ok(record) = rx.try_recv() {
            push_line(&mut buf, &record, key);
        }
        if out.write_all(&buf).await.is_err() || out.flush().await.is_err() {
            return;
        }
        buf.clear();
    }
    let count = dropped.load(Ordering::Relaxed);
    if count > 0 {
        let line = format!("{{\"event\":\"usage_log_dropped\",\"count\":{count}}}\n");
        let _ = out.write_all(line.as_bytes()).await;
        let _ = out.flush().await;
    }
}

/// One JSON line, serialized from borrowed fields: no per-record allocation
/// beyond growing the writer's reused buffer.
#[derive(Serialize)]
struct Line<'a> {
    event: &'static str,
    unix_ms: u64,
    path: &'a str,
    status: u16,
    outcome: &'static str,
    duration_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    usage_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<PromptRecord>,
}

fn push_line(buf: &mut Vec<u8>, r: &Record, prompt_key: Option<&HashKey>) {
    let usage = r
        .usage
        .as_ref()
        .map(|(usage, _)| *usage)
        .unwrap_or_default();
    let line = Line {
        event: "request",
        unix_ms: r.unix_ms as u64,
        path: r.info.path,
        status: r.status.as_u16(),
        outcome: r.outcome.as_str(),
        duration_ms: ms(r.duration),
        headers_ms: r.headers_after.map(ms),
        subject: r.info.identity.as_ref().map(|i| &*i.subject),
        session_id: r.info.session_id.as_ref().and_then(|v| v.to_str().ok()),
        model: r.model.as_deref(),
        usage_status: r
            .usage
            .as_ref()
            .map_or("not_observed", |(_, status)| status),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        prompt: r
            .info
            .prompt
            .as_ref()
            .zip(prompt_key)
            .map(|(slot, key)| PromptRecord::new(slot, key)),
    };
    // Serializing plain fields into a Vec cannot fail.
    let _ = serde_json::to_writer(&mut *buf, &line);
    buf.push(b'\n');
}

/// Milliseconds rounded to two decimals.
fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1000.0 * 100.0).round() / 100.0
}

/// Response body wrapper that observes usage and emits a record on drop.
pub(crate) struct Observed<B> {
    inner: B,
    /// Present for successful SSE responses only.
    scanner: Option<SseUsageScanner>,
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
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(scanner), Some(data)) = (this.scanner.as_mut(), frame.data_ref()) {
                    scanner.feed(data);
                }
                // With a known length, hyper stops polling once the body reports its end
                // and never sees `Ready(None)`.
                if this.inner.is_end_stream() {
                    this.outcome = Outcome::Completed;
                }
            }
            Poll::Ready(Some(Err(_))) => this.outcome = Outcome::Failed,
            Poll::Ready(None) => this.outcome = Outcome::Completed,
            Poll::Pending => {}
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
