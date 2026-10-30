//! Heap allocation metrics, from a Wye binary built with `--features alloc-stats`.
//!
//! The instrumented binary counts every allocation and serves the counters at
//! `GET /debug/alloc`; each scenario reads them before and after. Allocation
//! counts barely depend on machine load, so unlike CPU time they resolve
//! small changes: one extra allocation per streamed event shows up exactly.
//! The counter reads themselves allocate a constant few times per read,
//! negligible against thousands of requests.

use std::{collections::BTreeMap, path::Path, time::Duration};

use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::Value;

use crate::{
    client::Target,
    gateway_process::{Gateway, GatewayProcess},
    measure::{UPLOAD_BYTES, UPLOAD_CONCURRENCY},
    mock, scenarios,
    stats::put,
};

const UNARY_REQUESTS: usize = 10_000;
const CONCURRENCY: usize = 32;
/// Streams per SSE measurement; two stream lengths separate per-event from per-stream cost.
const SSE_STREAMS: usize = 200;
const SSE_SHORT_EVENTS: u32 = 10;
const SSE_LONG_EVENTS: u32 = 110;
const UPLOAD_REQUESTS: usize = 64;
const IDLE_STREAMS: usize = 1_000;
/// Idle streams whose request carried an agent-sized context.
const IDLE_AFTER_UPLOAD_STREAMS: usize = 200;
const IDLE_UPLOAD_BYTES: usize = 512 * 1024;
/// Clients that stop reading mid-stream, on the TLS path.
const STALLED_STREAMS: usize = 32;

#[derive(Clone, Copy)]
struct Counters {
    allocations: f64,
    allocated_bytes: f64,
    live_bytes: f64,
}

/// Live heap bytes of an alloc-stats build, or `None` for other builds.
pub(crate) async fn live_bytes(target: &Target) -> Option<f64> {
    let res = target
        .client()
        .request(target.get("/debug/alloc"))
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    let body = res.into_body().collect().await.ok()?.to_bytes();
    serde_json::from_slice::<Value>(&body).ok()?["live_bytes"].as_f64()
}

async fn counters(target: &Target) -> Counters {
    let res = target
        .client()
        .request(target.get("/debug/alloc"))
        .await
        .expect("instrumented wye unreachable");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body)
        .expect("no /debug/alloc: is the binary built with --features alloc-stats?");
    let field = |name: &str| json[name].as_f64().unwrap();
    Counters {
        allocations: field("allocations"),
        allocated_bytes: field("allocated_bytes"),
        live_bytes: field("live_bytes"),
    }
}

/// Allocations and allocated bytes while `work` runs.
async fn delta(target: &Target, work: impl Future<Output = ()>) -> (f64, f64) {
    let before = counters(target).await;
    work.await;
    let after = counters(target).await;
    (
        after.allocations - before.allocations,
        after.allocated_bytes - before.allocated_bytes,
    )
}

/// `UNARY_REQUESTS` requests; failures abort, so a broken setup cannot pass as cheap.
async fn unary(t: &Target) {
    no_failures(&scenarios::unary(t, CONCURRENCY, UNARY_REQUESTS).await);
}

/// A failed request allocates less than one that succeeds, so it would pass
/// as cheaper: allocation counts are measured over successes only.
fn no_failures(requests: &[Option<u64>]) {
    let failed = requests.iter().filter(|r| r.is_none()).count();
    assert_eq!(
        failed, 0,
        "{failed} requests failed during allocation measurement"
    );
}

/// [`no_failures`], for streams.
fn no_failed_streams(streams: &[scenarios::Stream]) {
    let failed = streams.iter().filter(|s| s.failed).count();
    assert_eq!(
        failed, 0,
        "{failed} streams failed during allocation measurement"
    );
}

/// Live heap per stream while `count` streams stay open and silent.
async fn idle_live_bytes(t: &Target, count: usize, body: Option<Bytes>) -> f64 {
    let live_before = counters(t).await.live_bytes;
    let held = scenarios::open_idle_streams_with_body(t, count, body).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let live_open = counters(t).await.live_bytes;
    drop(held);
    // Let the gateway release the closed streams before the next measurement.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (live_open - live_before) / count as f64
}

/// Live heap per stream while `count` clients have stopped reading a stream
/// the upstream sends as fast as flow control allows.
async fn stalled_live_bytes(t: &Target, count: usize) -> f64 {
    let live_before = counters(t).await.live_bytes;
    let held = scenarios::stalled_streams(t, count).await;
    assert_eq!(held.len(), count, "stalled streams failed to open");
    // Let every stalled stream fill its buffers and flow-control windows.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let live_open = counters(t).await.live_bytes;
    drop(held);
    tokio::time::sleep(Duration::from_millis(300)).await;
    (live_open - live_before) / count as f64
}

async fn agent_uploads(t: &Target) {
    no_failures(
        &scenarios::upload(
            t,
            UPLOAD_CONCURRENCY,
            UPLOAD_REQUESTS,
            mock::agent_json(UPLOAD_BYTES),
        )
        .await,
    );
}

/// Runs the allocation scenarios against `binary`: the full set in front of the
/// plain upstream, and idle-stream memory in front of the TLS + HTTP/2 upstream
/// (`tls_base_url`, trusting `tls_ca_pem`), the production connection shape.
pub async fn measure(
    binary: &str,
    upstream_base_url: &str,
    tls_base_url: &str,
    tls_ca_pem: &str,
    dir: &Path,
    metrics: &mut BTreeMap<String, f64>,
) {
    let wye =
        GatewayProcess::start(Gateway::Wye(binary), upstream_base_url, None, dir, "alloc").await;
    let t = wye.target();
    // Warm up pools, buffers, and lazily initialized state, so steady state is measured.
    scenarios::unary(t, CONCURRENCY, 2_000).await;
    scenarios::sse(t, CONCURRENCY, 64, 5, 64, 0).await;

    per_request(t, metrics, "unary", UNARY_REQUESTS, true, unary(t)).await;

    let streams = |events| async move {
        no_failed_streams(&scenarios::sse(t, CONCURRENCY, SSE_STREAMS, events, 64, 0).await);
    };
    let (short, _) = delta(t, streams(SSE_SHORT_EVENTS)).await;
    let (long, _) = delta(t, streams(SSE_LONG_EVENTS)).await;
    let per_event =
        (long - short) / (SSE_STREAMS as f64 * f64::from(SSE_LONG_EVENTS - SSE_SHORT_EVENTS));
    put(metrics, "", "alloc_sse_per_event", per_event);
    put(
        metrics,
        "",
        "alloc_sse_per_stream",
        short / SSE_STREAMS as f64 - per_event * f64::from(SSE_SHORT_EVENTS),
    );

    let body = mock::message_json(UPLOAD_BYTES);
    let uploads = async {
        no_failures(&scenarios::upload(t, UPLOAD_CONCURRENCY, UPLOAD_REQUESTS, body).await);
    };
    per_request(t, metrics, "upload", UPLOAD_REQUESTS, true, uploads).await;
    // The same size shaped like Claude Code's, for comparison with the
    // cache-diagnostics variant below.
    per_request(
        t,
        metrics,
        "agent_upload",
        UPLOAD_REQUESTS,
        true,
        agent_uploads(t),
    )
    .await;

    put(
        metrics,
        "",
        "alloc_idle_live_bytes_per_stream",
        idle_live_bytes(t, IDLE_STREAMS, None).await,
    );
    put(
        metrics,
        "",
        "alloc_idle_after_upload_live_bytes_per_stream",
        idle_live_bytes(
            t,
            IDLE_AFTER_UPLOAD_STREAMS,
            Some(mock::message_json(IDLE_UPLOAD_BYTES)),
        )
        .await,
    );
    drop(wye);

    let tls = GatewayProcess::start(
        Gateway::Wye(binary),
        tls_base_url,
        Some(tls_ca_pem),
        dir,
        "alloc-tls",
    )
    .await;
    let t = tls.target();
    scenarios::unary(t, CONCURRENCY, 2_000).await;
    put(
        metrics,
        "",
        "alloc_tls_idle_live_bytes_per_stream",
        idle_live_bytes(t, IDLE_STREAMS, None).await,
    );
    put(
        metrics,
        "",
        "alloc_tls_idle_after_upload_live_bytes_per_stream",
        idle_live_bytes(
            t,
            IDLE_AFTER_UPLOAD_STREAMS,
            Some(mock::message_json(IDLE_UPLOAD_BYTES)),
        )
        .await,
    );
    put(
        metrics,
        "",
        "alloc_tls_stalled_live_bytes_per_stream",
        stalled_live_bytes(t, STALLED_STREAMS).await,
    );
    drop(tls);

    // The variants' per-request costs, each in its own process, in this order.
    for (gateway, name, bytes, agent_upload) in [
        (Gateway::WyeAuth(binary), "auth", false, false),
        (Gateway::WyeModels(binary), "models", false, false),
        (Gateway::WyeBedrock(binary), "bedrock", true, false),
        (Gateway::WyeFailover(binary), "failover", true, true),
        (Gateway::WyeDiag(binary), "diag", true, true),
    ] {
        let process = GatewayProcess::start(gateway, upstream_base_url, None, dir, "alloc").await;
        let t = process.target();
        scenarios::unary(t, CONCURRENCY, 2_000).await;
        let unary_name = format!("{name}_unary");
        per_request(t, metrics, &unary_name, UNARY_REQUESTS, bytes, unary(t)).await;
        if agent_upload {
            let upload_name = format!("{name}_agent_upload");
            per_request(
                t,
                metrics,
                &upload_name,
                UPLOAD_REQUESTS,
                true,
                agent_uploads(t),
            )
            .await;
        }
    }
}

/// Puts `alloc_{name}_per_request`, and with `bytes` also
/// `alloc_{name}_bytes_per_request`, for the `requests` requests `work` makes.
async fn per_request(
    t: &Target,
    metrics: &mut BTreeMap<String, f64>,
    name: &str,
    requests: usize,
    bytes: bool,
    work: impl Future<Output = ()>,
) {
    let (allocations, total) = delta(t, work).await;
    let requests = requests as f64;
    put(
        metrics,
        "alloc_",
        &format!("{name}_per_request"),
        allocations / requests,
    );
    if bytes {
        put(
            metrics,
            "alloc_",
            &format!("{name}_bytes_per_request"),
            total / requests,
        );
    }
}
