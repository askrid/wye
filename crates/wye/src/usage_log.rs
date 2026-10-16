//! Per-request usage records, written off the request path.
//!
//! The response body is wrapped in [`Observed`], which feeds the forwarded
//! bytes to a [`SseUsageScanner`](gateway_core::usage::SseUsageScanner)
//! without changing or delaying them and emits one [`Record`] when the body is
//! dropped: after completion, error, or client disconnect. Records go through
//! a bounded channel to one writer task, which writes them as JSON lines and,
//! with `[telemetry]`, exports them as OTLP logs; when the channel is full the
//! record is dropped and counted, so a slow sink never slows a stream.

mod agent;
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

use gateway_core::{agent::CacheMiss, credential::Identity, prompt::HashKey, usage::Usage};
use http::{HeaderValue, StatusCode};
use serde::Serialize;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};

use crate::{
    diagnostics::{PromptRecord, PromptSlot},
    gauge::Gauge,
};

pub(crate) use agent::AgentHeaders;
pub(crate) use observed::Observed;
pub(crate) use otlp::{OtlpTarget, parse_headers};

/// Records buffered before new ones are dropped.
const QUEUE_CAPACITY: usize = 4096;
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
    pub path: &'static str,
    pub session_id: Option<HeaderValue>,
    /// W3C `traceparent`, the parent of the request's OTLP span.
    pub traceparent: Option<HeaderValue>,
    /// Claude Code's agent structure and gateway hint headers.
    pub agent: AgentHeaders,
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

/// What the writer task writes besides the stderr log.
pub(crate) struct Sinks {
    /// Keys the prompt hashes of records that carry them.
    pub(crate) prompt_key: Option<Arc<HashKey>>,
    /// Exports records as OTLP logs and aggregates them into OTLP metrics.
    pub(crate) otlp: Option<Arc<OtlpTarget>>,
    /// Requests in flight, reported as `wye.requests.active`.
    pub(crate) active: Gauge,
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
    sinks: Sinks,
) where
    W: AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(4096);
    let key = sinks.prompt_key.as_deref();
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
        let mut record = Some(record);
        // Batch whatever else is queued into one write.
        while let Some(r) = record.take().or_else(|| rx.try_recv().ok()) {
            let line = line(&r, key);
            push_line(&mut buf, &line);
            if let Some((batch, metrics)) = otlp.as_mut() {
                metrics.record(&line);
                batch.push(&line);
                if batch.is_full() {
                    batch.flush();
                }
            }
        }
        if out.write_all(&buf).await.is_err() || out.flush().await.is_err() {
            return;
        }
        buf.clear();
    }
    let mut lost = Vec::new();
    if let Some((mut batch, metrics)) = otlp {
        batch.flush();
        export_metrics(&batch, &metrics);
        let otlp_dropped = batch.dropped();
        drop(batch);
        if let Some(exporter) = exporter {
            let _ = exporter.await;
        }
        if otlp_dropped > 0 {
            lost.push(format!(
                "{{\"event\":\"otlp_dropped\",\"records\":{otlp_dropped}}}\n"
            ));
        }
    }
    let count = dropped.load(Ordering::Relaxed);
    if count > 0 {
        lost.push(format!(
            "{{\"event\":\"usage_log_dropped\",\"count\":{count}}}\n"
        ));
    }
    for line in lost {
        let _ = out.write_all(line.as_bytes()).await;
    }
    let _ = out.flush().await;
}

fn unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
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
    /// Only for the OTLP span; not part of the JSON line.
    #[serde(skip)]
    traceparent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_class: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_compacted: Option<&'static str>,
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
    /// Why the request wrote the prompt cache without reading it.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_miss: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<PromptRecord>,
}

/// The record as borrowed fields, written as a JSON line and as OTLP.
fn line<'a>(r: &'a Record, prompt_key: Option<&HashKey>) -> Line<'a> {
    let usage = r
        .usage
        .as_ref()
        .map(|(usage, _)| *usage)
        .unwrap_or_default();
    let agent = r.info.agent.fields();
    Line {
        event: "request",
        unix_ms: r.unix_ms as u64,
        path: r.info.path,
        status: r.status.as_u16(),
        outcome: r.outcome.as_str(),
        duration_ms: ms(r.duration),
        headers_ms: r.headers_after.map(ms),
        subject: r.info.identity.as_ref().map(|i| &*i.subject),
        session_id: r.info.session_id.as_ref().and_then(|v| v.to_str().ok()),
        traceparent: r.info.traceparent.as_ref().and_then(|v| v.to_str().ok()),
        request_class: agent.request_class,
        agent_type: agent.agent_type,
        agent_id: agent.agent_id,
        parent_agent_id: agent.parent_agent_id,
        prompt_id: agent.prompt_id,
        compaction: agent.compaction.map(|c| c.as_str()),
        context_compacted: agent.context_compacted.map(|c| c.as_str()),
        model: r.model.as_deref(),
        usage_status: r
            .usage
            .as_ref()
            .map_or("not_observed", |(_, status)| status),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_miss: CacheMiss::classify(
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
            agent.compaction,
            agent.context_compacted,
        )
        .map(CacheMiss::as_str),
        prompt: r
            .info
            .prompt
            .as_ref()
            .zip(prompt_key)
            .map(|(slot, key)| PromptRecord::new(slot, key)),
    }
}

fn push_line(buf: &mut Vec<u8>, line: &Line<'_>) {
    // Serializing plain fields into a Vec cannot fail.
    let _ = serde_json::to_writer(&mut *buf, line);
    buf.push(b'\n');
}

/// Milliseconds rounded to two decimals.
fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1000.0 * 100.0).round() / 100.0
}
