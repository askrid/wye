//! Measuring one gateway process against the direct baseline of one upstream path.

use std::{collections::BTreeMap, time::Duration};

use serde_json::{Value, json};

use crate::{
    client::Target,
    gateway_process::GatewayProcess,
    mock,
    scenarios::{self, Stream},
    stats::{PERCENTILES, percentile, phase, put, sorted},
};

const UNARY_CONCURRENCY: usize = 32;
const UNARY_TOTAL: usize = 20_000;
const SSE_CONCURRENCY: usize = 64;
const SSE_STREAMS: usize = 640;
const SSE_EVENTS: u32 = 50;
const SSE_INTERVAL_US: u64 = 2_000;
const THROUGHPUT_CONCURRENCY: usize = 8;
const THROUGHPUT_STREAMS: usize = 32;
const THROUGHPUT_EVENTS: u32 = 2_000;
const THROUGHPUT_EVENT_BYTES: usize = 4_096;
const IDLE_STREAMS: usize = 1_000;
/// Agent-sized request bodies: Claude Code resends its whole context each turn.
const UPLOAD_BYTES: usize = 1024 * 1024;
const UPLOAD_CONCURRENCY: usize = 16;
const UPLOAD_REQUESTS: usize = 256;
/// Token-paced streams at scale: 50 events/s of ~100 bytes, like model output.
const PACED_STREAMS: usize = 2_000;
const PACED_EVENTS: u32 = 100;
const PACED_INTERVAL_US: u64 = 20_000;
const PACED_EVENT_BYTES: usize = 100;
/// Stream starts are spread over this time (one per millisecond): macOS caps
/// the listen backlog at 128, and over HTTP/1.1 every stream needs its own
/// upstream connection, so faster bursts would measure SYN retries.
const PACED_RAMP: Duration = Duration::from_millis(2_000);
const CANCEL_ROUNDS: usize = 20;
/// Requests whose total gateway CPU gives the per-request cost.
const CPU_PER_REQUEST_TOTAL: usize = 20_000;

/// Scenario parameters, recorded in the report so results stay interpretable.
pub(crate) fn scenario_report() -> Value {
    json!({
        "unary": {"concurrency": UNARY_CONCURRENCY, "requests": UNARY_TOTAL, "response_bytes": 512},
        "sse": {"concurrency": SSE_CONCURRENCY, "streams": SSE_STREAMS, "events": SSE_EVENTS, "interval_us": SSE_INTERVAL_US},
        "throughput": {"concurrency": THROUGHPUT_CONCURRENCY, "streams": THROUGHPUT_STREAMS, "events": THROUGHPUT_EVENTS, "event_bytes": THROUGHPUT_EVENT_BYTES},
        "idle": {"streams": IDLE_STREAMS},
        "cancel": {"rounds": CANCEL_ROUNDS},
        "upload": {"concurrency": UPLOAD_CONCURRENCY, "requests": UPLOAD_REQUESTS, "body_bytes": UPLOAD_BYTES},
        "paced": {"streams": PACED_STREAMS, "events": PACED_EVENTS, "interval_us": PACED_INTERVAL_US, "event_bytes": PACED_EVENT_BYTES, "ramp_ms": PACED_RAMP.as_millis() as u64},
    })
}

/// The direct baseline of one path: what a client sees without a gateway.
pub(crate) struct Direct {
    pub(crate) latency: Latency,
    pub(crate) mb_s: f64,
    upload: (Vec<u64>, usize),
    paced: Option<Paced>,
}

impl Direct {
    pub(crate) async fn measure(target: &Target, paced: bool) -> Self {
        let latency = Latency::measure(target).await;
        let (mb_s, _) = throughput(target).await;
        let upload = upload(target).await;
        let paced = if paced {
            Some(Paced::measure(target, || {}).await)
        } else {
            None
        };
        Self {
            latency,
            mb_s,
            upload,
            paced,
        }
    }
}

/// Which scenarios apply to a gateway on a path.
#[derive(Clone, Copy)]
pub(crate) struct Scope {
    /// The gateway relays SSE events as they arrive (idle and cancel scenarios).
    pub(crate) streams_sse: bool,
    /// The path runs the token-paced scenario.
    pub(crate) paced: bool,
    /// Only per-request scenarios (latency and CPU per request): for variants
    /// whose cost is per request, such as credential verification.
    pub(crate) per_request_only: bool,
}

/// Measures `process` against the `direct` baseline, adding `prefix`ed metrics.
///
/// Scenario order matters for memory metrics: idle streams are measured before
/// the upload and paced scenarios, whose allocations would otherwise distort them.
pub(crate) async fn measure(
    direct: &Direct,
    process: &GatewayProcess,
    prefix: &str,
    scope: Scope,
    metrics: &mut BTreeMap<String, f64>,
) {
    let via = phase(
        format!("{prefix}latency"),
        Latency::measure(process.target()),
    )
    .await;
    let cpu_before = process.cpu_secs();
    let (_, failed) = successes(
        phase(
            format!("{prefix}cpu-per-request"),
            scenarios::unary(process.target(), UNARY_CONCURRENCY, CPU_PER_REQUEST_TOTAL),
        )
        .await,
    );
    let cpu_after = process.cpu_secs();
    // `ps` reports CPU time in 10 ms steps; 20,000 requests make that ~0.5 µs per request.
    put(
        metrics,
        prefix,
        "unary_cpu_us_per_request",
        (cpu_after - cpu_before) * 1e6 / (CPU_PER_REQUEST_TOTAL - failed).max(1) as f64,
    );

    let base = &direct.latency;
    put(
        metrics,
        prefix,
        "unary_failed_requests",
        via.unary_failed as f64,
    );
    for (label, p) in PERCENTILES {
        let added = |via: &[u64], direct: &[u64]| percentile(via, p) - percentile(direct, p);
        put(
            metrics,
            prefix,
            &format!("via_unary_{label}_us"),
            percentile(&via.unary, p),
        );
        put(
            metrics,
            prefix,
            &format!("unary_added_{label}_us"),
            added(&via.unary, &base.unary),
        );
        put(
            metrics,
            prefix,
            &format!("sse_ttfb_added_{label}_us"),
            added(&via.ttfb, &base.ttfb),
        );
        put(
            metrics,
            prefix,
            &format!("sse_event_added_{label}_us"),
            added(&via.event_delay, &base.event_delay),
        );
    }
    if scope.per_request_only {
        return;
    }
    let cpu_before = process.cpu_secs();
    let (mb_s, mb) = phase(format!("{prefix}throughput"), throughput(process.target())).await;
    let cpu_after = process.cpu_secs();
    put(metrics, prefix, "via_throughput_mb_s", mb_s);
    // `ps` reports CPU time in 10 ms steps; the scenario streams enough data to make that negligible.
    put(
        metrics,
        prefix,
        "throughput_cpu_ms_per_mb",
        (cpu_after - cpu_before) * 1_000.0 / mb,
    );

    if scope.streams_sse {
        phase(
            format!("{prefix}idle+cancel"),
            measure_idle_and_cancel(process, prefix, metrics),
        )
        .await;
    }

    let cpu_before = process.cpu_secs();
    let (via_upload, upload_failed) =
        phase(format!("{prefix}upload"), upload(process.target())).await;
    put(
        metrics,
        prefix,
        "upload_failed_requests",
        upload_failed as f64,
    );
    let cpu_after = process.cpu_secs();
    for (label, p) in PERCENTILES {
        let added = percentile(&via_upload, p) - percentile(&direct.upload.0, p);
        put(metrics, prefix, &format!("upload_added_{label}_us"), added);
    }
    let uploaded_mb = (UPLOAD_BYTES * UPLOAD_REQUESTS) as f64 / 1e6;
    put(
        metrics,
        prefix,
        "upload_cpu_ms_per_mb",
        (cpu_after - cpu_before) * 1_000.0 / uploaded_mb,
    );

    if let (true, Some(direct_paced)) = (scope.paced, &direct.paced) {
        let rss_before = process.rss_kb();
        let cpu_before = process.cpu_secs();
        let mut rss_loaded = 0.0;
        let paced = phase(
            format!("{prefix}paced"),
            Paced::measure(process.target(), || rss_loaded = process.rss_kb()),
        )
        .await;
        let cpu_after = process.cpu_secs();
        for (label, p) in PERCENTILES {
            let added =
                percentile(&paced.event_delay, p) - percentile(&direct_paced.event_delay, p);
            put(
                metrics,
                prefix,
                &format!("paced_event_added_{label}_us"),
                added,
            );
        }
        put(
            metrics,
            prefix,
            "paced_ttfb_added_p99_us",
            percentile(&paced.ttfb, 99.0) - percentile(&direct_paced.ttfb, 99.0),
        );
        put(
            metrics,
            prefix,
            "paced_cpu_us_per_event",
            (cpu_after - cpu_before) * 1e6 / paced.event_delay.len() as f64,
        );
        put(
            metrics,
            prefix,
            "paced_failed_streams",
            paced.failed_streams as f64,
        );
        put(
            metrics,
            prefix,
            "paced_rss_kb_per_stream",
            (rss_loaded - rss_before) / PACED_STREAMS as f64,
        );
    }

    put(metrics, prefix, "rss_after_load_kb", process.rss_kb());
}

/// Memory per open idle stream, and upstream release after a client disconnect.
async fn measure_idle_and_cancel(
    process: &GatewayProcess,
    prefix: &str,
    metrics: &mut BTreeMap<String, f64>,
) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rss_before = process.rss_kb();
    let held = scenarios::open_idle_streams(process.target(), IDLE_STREAMS).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rss_open = process.rss_kb();
    drop(held);
    put(
        metrics,
        prefix,
        "idle_rss_kb_per_stream",
        (rss_open - rss_before) / IDLE_STREAMS as f64,
    );
    put(metrics, prefix, "rss_idle_kb", rss_before);

    let cancel = sorted(scenarios::cancel_latency(process.target(), CANCEL_ROUNDS).await);
    put(
        metrics,
        prefix,
        "cancel_upstream_release_p50_us",
        percentile(&cancel, 50.0),
    );
    put(
        metrics,
        prefix,
        "cancel_upstream_release_max_us",
        percentile(&cancel, 100.0),
    );
}

/// Sorted latency samples of the unary and SSE scenarios.
pub(crate) struct Latency {
    pub(crate) unary: Vec<u64>,
    pub(crate) unary_failed: usize,
    pub(crate) ttfb: Vec<u64>,
    pub(crate) event_delay: Vec<u64>,
}

impl Latency {
    async fn measure(target: &Target) -> Self {
        // Warm up connection pools and allocators.
        scenarios::unary(target, UNARY_CONCURRENCY, 2_000).await;
        scenarios::sse(target, SSE_CONCURRENCY, 128, 5, 64, 0).await;

        let (unary, unary_failed) =
            successes(scenarios::unary(target, UNARY_CONCURRENCY, UNARY_TOTAL).await);
        let streams = scenarios::sse(
            target,
            SSE_CONCURRENCY,
            SSE_STREAMS,
            SSE_EVENTS,
            128,
            SSE_INTERVAL_US,
        )
        .await;
        Self {
            unary,
            unary_failed,
            ttfb: sorted(streams.iter().map(|s| s.ttfb_us).collect()),
            event_delay: sorted(
                streams
                    .into_iter()
                    .flat_map(|s| s.event_delays_us)
                    .collect(),
            ),
        }
    }
}

/// Latency samples of agent-sized uploads.
/// Sorted latencies of agent-sized uploads, and the number that failed.
async fn upload(target: &Target) -> (Vec<u64>, usize) {
    let body = mock::padded_json(
        r#"{"model":"bench","max_tokens":1,"messages":[{"role":"user","content":""#,
        r#""}]}"#,
        UPLOAD_BYTES,
    );
    successes(scenarios::upload(target, UPLOAD_CONCURRENCY, UPLOAD_REQUESTS, body).await)
}

/// Sorted latencies of successful requests, and the number that failed.
fn successes(samples: Vec<Option<u64>>) -> (Vec<u64>, usize) {
    let total = samples.len();
    let ok = sorted(samples.into_iter().flatten().collect());
    let failed = total - ok.len();
    (ok, failed)
}

/// Sorted samples of the token-paced scenario.
struct Paced {
    ttfb: Vec<u64>,
    event_delay: Vec<u64>,
    failed_streams: usize,
}

impl Paced {
    /// Runs all streams concurrently; `while_loaded` runs once every stream is open.
    async fn measure(target: &Target, while_loaded: impl FnOnce()) -> Self {
        let streams = {
            let target = target.clone();
            tokio::spawn(async move {
                scenarios::paced(
                    &target,
                    PACED_STREAMS,
                    PACED_RAMP,
                    PACED_EVENTS,
                    PACED_EVENT_BYTES,
                    PACED_INTERVAL_US,
                )
                .await
            })
        };
        // Each stream lasts 2 s and starts within the 2 s ramp: just after the
        // ramp, every stream is open and streaming.
        tokio::time::sleep(PACED_RAMP + Duration::from_millis(100)).await;
        while_loaded();
        let streams = streams.await.unwrap();
        let failed_streams = streams.iter().filter(|s| s.failed).count();
        Self {
            ttfb: sorted(streams.iter().map(|s| s.ttfb_us).collect()),
            event_delay: sorted(
                streams
                    .into_iter()
                    .flat_map(|s| s.event_delays_us)
                    .collect(),
            ),
            failed_streams,
        }
    }
}

/// Returns (MB/s, MB streamed) for large back-to-back events.
async fn throughput(target: &Target) -> (f64, f64) {
    let start = std::time::Instant::now();
    let streams: Vec<Stream> = scenarios::sse(
        target,
        THROUGHPUT_CONCURRENCY,
        THROUGHPUT_STREAMS,
        THROUGHPUT_EVENTS,
        THROUGHPUT_EVENT_BYTES,
        0,
    )
    .await;
    let mb = streams.iter().map(|s| s.bytes).sum::<u64>() as f64 / 1e6;
    (mb / start.elapsed().as_secs_f64(), mb)
}
