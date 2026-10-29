//! Per-request usage records, written off the request path.
//!
//! The response body is wrapped in [`Observed`], which feeds the forwarded
//! bytes to the API's [`UsageScanner`](gateway_core::usage::UsageScanner)
//! without changing or delaying them and emits one [`Record`] when the body is
//! dropped: after completion, error, or client disconnect. Records go through
//! a bounded channel to one writer task, which writes them as JSON lines and,
//! with `[telemetry]`, exports them as OTLP logs, metrics, and spans; when the
//! channel is full the record is dropped and counted, so a slow sink never
//! slows a stream.

mod agent;
mod line;
mod metrics;
mod observed;
mod otlp;

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gateway_core::{
    api::Endpoint,
    credential::Identity,
    pricing::Pricing,
    prompt::HashKey,
    usage::{ToolCalls, Usage},
};
use http::{HeaderValue, StatusCode};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};

use crate::{diagnostics::PromptSlot, gauge::Gauge, upgrade_check::UpgradeSlot};

pub(crate) use agent::AgentHeaders;
use line::{Line, line, push_line};
pub(crate) use observed::{BodyKind, Observed};
pub(crate) use otlp::{OtlpTarget, parse_headers};

/// Records buffered before new ones are dropped.
const QUEUE_CAPACITY: usize = 4096;
/// The status of a request the client left before it was answered.
const CLIENT_CLOSED: u16 = 499;
/// Queued records batched into one write at most: bounds the write buffer,
/// and lets the export ticks run while records keep arriving.
const WRITE_BATCH: usize = 256;
/// A partly filled OTLP batch is sent after at most this long.
const OTLP_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
/// Cumulative OTLP metrics are exported this often.
const OTLP_METRICS_INTERVAL: Duration = Duration::from_secs(10);

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
    /// The proxied endpoint: its path and API.
    pub endpoint: Endpoint,
    pub session_id: Option<HeaderValue>,
    /// W3C `traceparent`, the parent of the request's OTLP span.
    pub traceparent: Option<HeaderValue>,
    /// Claude Code's agent structure and gateway hint headers.
    pub agent: AgentHeaders,
    /// Verified gateway credential, when credentials are configured.
    pub identity: Option<Arc<Identity>>,
    /// Filled with the request body's prompt hashes when cache diagnostics are on.
    pub prompt: Option<PromptSlot>,
    /// The alias the request named, when one was resolved.
    pub requested_model: Option<Arc<str>>,
    /// Filled with the request body's upgrade-check findings when sampled.
    pub upgrade: Option<UpgradeSlot>,
    /// The route upstream the request went to, when the route names them.
    pub upstream: Option<Arc<str>>,
    /// Upstreams that refused the request before the one that answered.
    pub failovers: Vec<RefusedAttempt>,
    pub started: Instant,
}

/// An upstream that refused a request, which then went to the next one.
pub(crate) struct RefusedAttempt {
    pub upstream: Option<Arc<str>>,
    /// The refusal's status; `None`: the connection could not be made.
    pub status: Option<u16>,
}

pub(crate) struct Record {
    info: RequestInfo,
    status: StatusCode,
    headers_after: Option<Duration>,
    duration: Duration,
    outcome: Outcome,
    model: Option<Box<str>>,
    usage: Option<(Usage, &'static str)>,
    /// `tool_use` blocks the response started.
    tool_calls: Option<Box<ToolCalls>>,
    unix_ms: u128,
}

/// What the writer task writes besides the stderr log.
pub(crate) struct Sinks {
    /// Keys the prompt hashes of records that carry them.
    pub(crate) prompt_key: Option<Arc<HashKey>>,
    /// Exports records as OTLP logs and spans and aggregates them into OTLP metrics.
    pub(crate) otlp: Option<Arc<OtlpTarget>>,
    /// Requests in flight, reported as `wye.requests.active`.
    pub(crate) active: Gauge,
    /// Prices each record's usage, when `[pricing]` is configured.
    pub(crate) pricing: Option<Arc<Pricing>>,
    /// Upgrade-check targets, when `[upgrade_check]` is configured.
    pub(crate) upgrade_targets: Option<Arc<[Box<str>]>>,
}

/// Cloneable handle that queues records for the writer task.
#[derive(Clone)]
pub(crate) struct UsageLog {
    tx: mpsc::Sender<Record>,
    dropped: Arc<AtomicU64>,
}

impl UsageLog {
    /// Starts the writer task. It ends after every handle is dropped and the queue is written.
    pub(crate) fn start<W>(out: W, sinks: Sinks) -> (Self, JoinHandle<()>)
    where
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer = tokio::spawn(write_records(rx, out, Arc::clone(&dropped), sinks));
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
        self.unobserved(info, status, Outcome::GatewayError);
    }

    /// A record without a response body to observe.
    fn unobserved(&self, info: RequestInfo, status: StatusCode, outcome: Outcome) {
        let duration = info.started.elapsed();
        self.send(Record {
            info,
            status,
            headers_after: None,
            duration,
            outcome,
            model: None,
            usage: None,
            tool_calls: None,
            unix_ms: unix_ms(),
        });
    }
}

/// A request's facts until it is answered. Dropped before that, with the
/// future of a client that left while Wye held its body or waited for the
/// upstream's headers, it records the request as cancelled, with nginx's
/// `499` (client closed request) as its status: the provider may have
/// started, and may bill, the request.
pub(crate) struct Unanswered<'a> {
    info: Option<RequestInfo>,
    log: &'a UsageLog,
}

impl<'a> Unanswered<'a> {
    pub(crate) fn new(info: RequestInfo, log: &'a UsageLog) -> Self {
        Self {
            info: Some(info),
            log,
        }
    }

    pub(crate) fn info(&mut self) -> &mut RequestInfo {
        match &mut self.info {
            Some(info) => info,
            None => unreachable!("only `answered` takes the facts, and it consumes the guard"),
        }
    }

    /// The facts, for the record of the answer.
    pub(crate) fn answered(mut self) -> RequestInfo {
        match self.info.take() {
            Some(info) => info,
            None => unreachable!("only `answered` takes the facts, and it consumes the guard"),
        }
    }
}

impl Drop for Unanswered<'_> {
    fn drop(&mut self) {
        if let Some(info) = self.info.take() {
            let status = StatusCode::from_u16(CLIENT_CLOSED).unwrap_or(StatusCode::REQUEST_TIMEOUT);
            self.log.unobserved(info, status, Outcome::Cancelled);
        }
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
    sinks: Sinks,
) where
    W: AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(4096);
    let context = line::Context {
        prompt_key: sinks.prompt_key.as_deref(),
        pricing: sinks.pricing.as_deref(),
        upgrade_targets: sinks.upgrade_targets.as_deref(),
    };
    // The exporter task is owned here and finishes its queue after the loop.
    let (mut otlp, exporter) = match sinks.otlp {
        Some(target) => {
            let (batch, exporter) = otlp::Batch::start(target);
            let metrics = metrics::Metrics::new(unix_nanos());
            (Some((batch, metrics)), Some(exporter))
        }
        None => (None, None),
    };
    let export_metrics = |batch: &otlp::Batch, metrics: &metrics::Metrics| {
        let lost = batch
            .dropped()
            .saturating_add(dropped.load(Ordering::Relaxed));
        let body = metrics.encode(
            &batch.target().resource,
            unix_nanos(),
            sinks.active.current(),
            lost,
        );
        batch.send_metrics(body);
    };
    // The first ticks come one period after start, not immediately.
    let after = |period| tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    let mut logs_tick = after(OTLP_FLUSH_INTERVAL);
    logs_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut metrics_tick = after(OTLP_METRICS_INTERVAL);
    metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // Cancel safety: `recv` and the ticks lose nothing when another branch wins.
        let record = tokio::select! {
            record = rx.recv() => record,
            _ = logs_tick.tick(), if otlp.is_some() => {
                if let Some((batch, _)) = otlp.as_mut() {
                    batch.flush();
                }
                continue;
            }
            _ = metrics_tick.tick(), if otlp.is_some() => {
                if let Some((batch, metrics)) = otlp.as_ref() {
                    export_metrics(batch, metrics);
                }
                continue;
            }
        };
        let Some(record) = record else { break };
        let mut next = Some(record);
        // Batch what else is queued into one write.
        for _ in 0..WRITE_BATCH {
            let Some(r) = next.take().or_else(|| rx.try_recv().ok()) else {
                break;
            };
            let line = line(&r, &context);
            push_line(&mut buf, &line);
            if let Some((batch, metrics)) = otlp.as_mut() {
                metrics.record(&line);
                batch.push(&line);
                if batch.is_full() {
                    batch.flush();
                }
            }
        }
        // A closed log ends the writer, through the shutdown below, so the
        // exporter task still finishes its queue and is awaited.
        if out.write_all(&buf).await.is_err() || out.flush().await.is_err() {
            break;
        }
        buf.clear();
    }
    // The log's own losses, written before the exporter's last exports,
    // which a slow collector can hold past shutdown's flush.
    let count = dropped.load(Ordering::Relaxed);
    if count > 0 {
        let line = format!("{{\"event\":\"usage_log_dropped\",\"count\":{count}}}\n");
        let _ = out.write_all(line.as_bytes()).await;
        let _ = out.flush().await;
    }
    if let Some((mut batch, metrics)) = otlp {
        batch.flush();
        export_metrics(&batch, &metrics);
        let otlp_lost = batch.lost();
        drop(batch);
        if let Some(exporter) = exporter {
            let _ = exporter.await;
        }
        // After the exporter: the last exports' failures count too.
        let otlp_dropped = otlp_lost.load(Ordering::Relaxed);
        if otlp_dropped > 0 {
            let line = format!("{{\"event\":\"otlp_dropped\",\"records\":{otlp_dropped}}}\n");
            let _ = out.write_all(line.as_bytes()).await;
        }
    }
    let _ = out.flush().await;
}

fn unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}
