//! Workloads. Each returns raw samples; `run` turns them into metrics.

use std::{
    future::Future,
    time::{Duration, Instant},
};

use http_body_util::BodyExt;
use hyper::body::Incoming;
use tokio::{sync::mpsc, task::JoinSet};

use crate::{
    client::Target,
    mock::{now_nanos, report_drops_to},
};

/// Reported when an upstream stream is not released at all within this time.
pub const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-stream SSE sample.
pub struct Stream {
    pub ttfb_us: u64,
    /// Delay of each event from mock send to client receive.
    pub event_delays_us: Vec<u64>,
    pub bytes: u64,
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

/// Latency in µs of small non-streaming requests.
pub async fn unary(target: &Target, concurrency: usize, total: usize) -> Vec<u64> {
    let target = target.clone();
    run_workers(concurrency, total, move || {
        let target = target.clone();
        async move {
            let start = Instant::now();
            let req = target.messages(&[("x-bench-unary", "512".into())]);
            let res = target.client().request(req).await.unwrap();
            res.into_body().collect().await.unwrap();
            start.elapsed().as_micros() as u64
        }
    })
    .await
}

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
        async move {
            let start = Instant::now();
            let req = target.messages(&[
                ("x-bench-events", events.to_string()),
                ("x-bench-size", size.to_string()),
                ("x-bench-interval-us", interval_us.to_string()),
            ]);
            let mut body = target.client().request(req).await.unwrap().into_body();
            let mut ttfb_us = None;
            let mut event_delays_us = Vec::with_capacity(events as usize);
            let mut pending: Vec<u8> = Vec::new();
            let mut bytes = 0;
            while let Some(frame) = body.frame().await {
                let Ok(data) = frame.unwrap().into_data() else {
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
            Stream {
                ttfb_us: ttfb_us.unwrap_or(0),
                event_delays_us,
                bytes,
            }
        }
    })
    .await
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
    let mut set = JoinSet::new();
    for _ in 0..count {
        let target = target.clone();
        set.spawn(async move {
            let req = target.messages(&idle_stream_headers());
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
