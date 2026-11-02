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
mod writer;

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
    route::FailoverReason,
    usage::{ToolCalls, Usage, UsageStatus},
};
use http::{HeaderValue, Response, StatusCode};
use tokio::{io::AsyncWrite, sync::mpsc, task::JoinHandle};

use crate::{diagnostics::PromptSlot, gauge::Gauge, upgrade_check::UpgradeSlot};

pub(crate) use agent::AgentHeaders;
use line::Line;
pub(crate) use observed::Observed;
pub(crate) use otlp::OtlpTarget;

/// Records buffered before new ones are dropped.
const QUEUE_CAPACITY: usize = 4096;
/// The status of a request the client left before it was answered.
const CLIENT_CLOSED: u16 = 499;

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
    pub(crate) fn as_str(self) -> &'static str {
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
    /// Agent headers: Claude Code's structure and hints, or Codex's mapped
    /// onto the same fields.
    pub agent: AgentHeaders,
    /// Verified gateway token, when tokens are configured.
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
    pub reason: FailoverReason,
}

pub(crate) struct Record {
    info: RequestInfo,
    status: StatusCode,
    duration: Duration,
    outcome: Outcome,
    seen: Seen,
    unix_ms: u64,
}

/// What observing a response body showed; a request without one has nothing.
#[derive(Default)]
struct Seen {
    headers_after: Option<Duration>,
    model: Option<Box<str>>,
    usage: Option<(Usage, UsageStatus)>,
    /// Tool calls the response started.
    tool_calls: Option<Box<ToolCalls>>,
}

/// What the writer task writes besides the stdout log.
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
        let writer = tokio::spawn(writer::write_records(rx, out, Arc::clone(&dropped), sinks));
        (Self { tx, dropped }, writer)
    }

    fn send(&self, record: Record) {
        if self.tx.try_send(record).is_err() {
            // Statistics counter only; no ordering with other memory is needed.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record(&self, info: RequestInfo, status: StatusCode, outcome: Outcome, seen: Seen) {
        let duration = info.started.elapsed();
        self.send(Record {
            info,
            status,
            duration,
            outcome,
            seen,
            unix_ms: unix_ms(),
        });
    }

    /// Records `res`, a response Wye generated itself, and returns it.
    pub(crate) fn gateway_error<B>(&self, info: RequestInfo, res: Response<B>) -> Response<B> {
        self.record(info, res.status(), Outcome::GatewayError, Seen::default());
        res
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
            self.log
                .record(info, status, Outcome::Cancelled, Seen::default());
        }
    }
}

fn unix_ms() -> u64 {
    unix_nanos() / 1_000_000
}

fn unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}
