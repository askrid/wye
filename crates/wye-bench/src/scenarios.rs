//! Workloads. Each returns raw samples; `run` turns them into metrics.

use std::{
    future::Future,
    time::{Duration, Instant},
};

use bytes::Bytes;
use http::Request;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use tokio::{sync::mpsc, task::JoinSet};

use crate::{
    client::Target,
    mock::{now_nanos, report_drops_to},
};

/// Reported when an upstream stream is not released at all within this time.
pub const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
/// A stream that has not ended by then counts as failed: a gateway that
/// stalls streams is a result, not a hung benchmark (and a hung commit hook).
const STREAM_DEADLINE: Duration = Duration::from_secs(30);
/// The same for a non-streaming request, upload included.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);

/// Per-stream SSE sample.
pub struct Stream {
    pub ttfb_us: u64,
    /// Delay of each event from mock send to client receive.
    pub event_delays_us: Vec<u64>,
    pub bytes: u64,
    /// The request or body failed, or the stream ended before all its
    /// events.
    pub failed: bool,
}

/// Runs `total` calls of `f` on `concurrency` workers.
async fn run_workers<T, F, Fut>(concurrency: usize, total: usize, f: F) -> Vec<T>
where
    T: Send + 'static,
    F: Fn() -> Fut + Clone + Send + 'static,
    Fut: Future<Output = T> + Send,
{
    let mut set = JoinSet::new();
    for worker in 0..concurrency {
        let f = f.clone();
        let count = total / concurrency + usize::from(worker < total % concurrency);
        set.spawn(async move {
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                out.push(f().await);
            }
            out
        });
    }
    let mut all = Vec::with_capacity(total);
    while let Some(part) = set.join_next().await {
        all.extend(part.unwrap());
    }
    all
}

/// Latency in µs of small non-streaming requests; `None` for a failed request.
pub async fn unary(target: &Target, concurrency: usize, total: usize) -> Vec<Option<u64>> {
    let target = target.clone();
    run_workers(concurrency, total, move || {
        let target = target.clone();
        async move { timed(&target, target.messages(&[("x-bench-unary", "512".into())])).await }
    })
    .await
}

/// Latency in µs of requests uploading `body` (like a resent agent context),
/// each answered with a small response; `None` for a failed request.
pub async fn upload(
    target: &Target,
    concurrency: usize,
    total: usize,
    body: Bytes,
) -> Vec<Option<u64>> {
    let target = target.clone();
    run_workers(concurrency, total, move || {
        let target = target.clone();
        let body = body.clone();
        async move {
            let req = target.messages_with_body(&[("x-bench-unary", "512".into())], body);
            timed(&target, req).await
        }
    })
    .await
}

/// Time until the full response arrives, or `None` if the request or status
/// failed: a fast error must never count as a fast response.
async fn timed(target: &Target, req: Request<Full<Bytes>>) -> Option<u64> {
    let start = Instant::now();
    let answered = async {
        let res = target.client().request(req).await.ok()?;
        let ok = res.status().is_success();
        let complete = res.into_body().collect().await.is_ok();
        Some(ok && complete)
    };
    let ok = tokio::time::timeout(REQUEST_DEADLINE, answered)
        .await
        .ok()
        .flatten()?;
    ok.then(|| start.elapsed().as_micros() as u64)
}

/// SSE streams on `concurrency` workers, each worker reading its streams one after another.
pub async fn sse(
    target: &Target,
    concurrency: usize,
    total: usize,
    events: u32,
    size: usize,
    interval_us: u64,
) -> Vec<Stream> {
    let target = target.clone();
    run_workers(concurrency, total, move || {
        let target = target.clone();
        async move { read_stream(&target, events, size, interval_us).await }
    })
    .await
}

/// `streams` concurrent SSE streams whose starts are spread evenly over `ramp`,
/// as independent clients arrive, instead of all connecting in one instant.
pub async fn paced(
    target: &Target,
    streams: usize,
    ramp: Duration,
    events: u32,
    size: usize,
    interval_us: u64,
) -> Vec<Stream> {
    let mut set = JoinSet::new();
    for i in 0..streams {
        let target = target.clone();
        let delay = ramp.mul_f64(i as f64 / streams as f64);
        set.spawn(async move {
            tokio::time::sleep(delay).await;
            read_stream(&target, events, size, interval_us).await
        });
    }
    let mut out = Vec::with_capacity(streams);
    while let Some(stream) = set.join_next().await {
        out.push(stream.unwrap());
    }
    out
}

/// Requests one SSE stream from the mock and records its timings; failed if
/// it has not ended within [`STREAM_DEADLINE`].
async fn read_stream(target: &Target, events: u32, size: usize, interval_us: u64) -> Stream {
    tokio::time::timeout(
        STREAM_DEADLINE,
        stream_events(target, events, size, interval_us),
    )
    .await
    .unwrap_or(Stream {
        ttfb_us: 0,
        event_delays_us: Vec::new(),
        bytes: 0,
        failed: true,
    })
}

async fn stream_events(target: &Target, events: u32, size: usize, interval_us: u64) -> Stream {
    let start = Instant::now();
    let req = target.messages(&[
        ("x-bench-events", events.to_string()),
        ("x-bench-size", size.to_string()),
        ("x-bench-interval-us", interval_us.to_string()),
    ]);
    let mut ttfb_us = None;
    let mut event_delays_us = Vec::with_capacity(events as usize);
    let mut pending: Vec<u8> = Vec::new();
    let mut bytes = 0;
    let mut failed = false;
    // A gateway that drops streams is a result to report, not a reason to abort the run.
    let mut body = match target.client().request(req).await {
        Ok(res) if res.status().is_success() => Some(res.into_body()),
        _ => None,
    };
    if body.is_none() {
        failed = true;
    }
    while let Some(frame) = match body.as_mut() {
        Some(body) => body.frame().await,
        None => None,
    } {
        let Ok(frame) = frame else {
            failed = true;
            break;
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let received = now_nanos();
        ttfb_us.get_or_insert_with(|| start.elapsed().as_micros() as u64);
        bytes += data.len() as u64;
        pending.extend_from_slice(&data);
        while let Some(end) = find(&pending, b"\n\n") {
            let event: Vec<u8> = pending.drain(..end + 2).collect();
            if let Some(sent) = event_timestamp(&event) {
                event_delays_us.push(received.saturating_sub(sent) / 1_000);
            }
        }
    }
    // A stream that ends before all its events is as broken as one that
    // fails.
    if event_delays_us.len() < events as usize {
        failed = true;
    }
    Stream {
        ttfb_us: ttfb_us.unwrap_or(0),
        event_delays_us,
        bytes,
        failed,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn event_timestamp(event: &[u8]) -> Option<u64> {
    let start = find(event, b"\"t\":")? + 4;
    let digits = event[start..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    std::str::from_utf8(&event[start..start + digits])
        .ok()?
        .parse()
        .ok()
}

/// Opens `count` streams that stay open and silent; returns the held bodies.
pub async fn open_idle_streams(target: &Target, count: usize) -> Vec<Incoming> {
    open_idle_streams_with_body(target, count, None).await
}

/// Like `open_idle_streams`, each request carrying `body` (an agent's resent
/// context) instead of a small one.
pub async fn open_idle_streams_with_body(
    target: &Target,
    count: usize,
    body: Option<Bytes>,
) -> Vec<Incoming> {
    let mut set = JoinSet::new();
    for _ in 0..count {
        let target = target.clone();
        let body = body.clone();
        set.spawn(async move {
            let req = match body {
                Some(body) => target.messages_with_body(&idle_stream_headers(), body),
                None => target.messages(&idle_stream_headers()),
            };
            let mut body = target.client().request(req).await.unwrap().into_body();
            body.frame().await.unwrap().unwrap();
            body
        });
    }
    let mut bodies = Vec::with_capacity(count);
    while let Some(body) = set.join_next().await {
        bodies.push(body.unwrap());
    }
    bodies
}

/// One event, then silence for 60 s, like a model thinking.
fn idle_stream_headers() -> Vec<(&'static str, String)> {
    vec![
        ("x-bench-events", "2".into()),
        ("x-bench-size", "16".into()),
        ("x-bench-interval-us", "60000000".into()),
    ]
}

/// Time in µs from the client dropping a silent stream to the upstream releasing it.
///
/// The gateway has no upstream write on which to notice the disconnect. Runs
/// sequentially so each drop report belongs to the current stream.
pub async fn cancel_latency(target: &Target, rounds: usize) -> Vec<u64> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    report_drops_to(tx);
    let mut out = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        // A fresh pool per round: otherwise the dropped stream's connection could be reused.
        let target = target.fresh();
        let mut headers = idle_stream_headers();
        headers.push(("x-bench-report-drop", "1".into()));
        let mut body = target
            .client()
            .request(target.messages(&headers))
            .await
            .unwrap()
            .into_body();
        body.frame().await.unwrap().unwrap();
        let cancelled = now_nanos();
        drop(body);
        drop(target);
        out.push(
            match tokio::time::timeout(CANCEL_TIMEOUT, rx.recv()).await {
                Ok(Some(dropped)) => dropped.saturating_sub(cancelled) / 1_000,
                _ => CANCEL_TIMEOUT.as_micros() as u64,
            },
        );
        // Discard reports from a late drop so the next round starts clean.
        while rx.try_recv().is_ok() {}
    }
    out
}

/// Small requests, each on a new connection, all at once: what the first
/// requests after a start or a scale-out pay to open connections (on the TLS
/// path, a handshake). `None` for a failed request.
pub async fn cold_unary(target: &Target, count: usize) -> Vec<Option<u64>> {
    let mut set = JoinSet::new();
    for _ in 0..count {
        let target = target.fresh();
        set.spawn(async move {
            let req = target.messages(&[("x-bench-unary", "512".into())]);
            timed(&target, req).await
        });
    }
    let mut out = Vec::with_capacity(count);
    while let Some(sample) = set.join_next().await {
        out.push(sample.unwrap());
    }
    out
}

/// Opens `count` streams that send as fast as flow control allows, reads one
/// frame of each, then stops reading: clients that stall mid-stream. Each is
/// its own client with its own connection, as real clients are; the held
/// bodies keep the streams open until dropped.
pub async fn stalled_streams(target: &Target, count: usize) -> Vec<Incoming> {
    let mut set = JoinSet::new();
    for _ in 0..count {
        let target = target.fresh();
        set.spawn(async move {
            let req = target.messages(&[
                ("x-bench-events", "2000".into()),
                ("x-bench-size", "4096".into()),
                ("x-bench-interval-us", "0".into()),
            ]);
            let mut body = target.client().request(req).await.ok()?.into_body();
            body.frame().await?.ok()?;
            Some(body)
        });
    }
    let mut bodies = Vec::with_capacity(count);
    while let Some(body) = set.join_next().await {
        bodies.extend(body.unwrap());
    }
    bodies
}
