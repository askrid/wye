//! Measuring one gateway process against the direct baseline of one upstream path.

use std::{collections::BTreeMap, time::Duration};

use serde_json::{Value, json};

mod paced;
mod stress;
mod upload;

use upload::{
    AGENT_UPLOAD_REQUESTS, UPLOAD_BYTES, UPLOAD_CONCURRENCY, UPLOAD_REQUESTS, agent_body,
    measure_uploads, upload, upload_agent, upload_body,
};

use paced::{Paced, measure_paced};
use stress::{Cold, Stress, measure_cold, measure_stress};

use crate::{
    client::Target,
    gateway_process::GatewayProcess,
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
const CANCEL_ROUNDS: usize = 20;
/// Requests whose total gateway CPU gives the per-request cost.
const CPU_PER_REQUEST_TOTAL: usize = 20_000;
/// Token-sized events, unpaced, whose total gateway CPU gives the cost per
/// event: what the usage scanner of each API adds to every streamed event.
const EVENT_CPU_CONCURRENCY: usize = 32;
const EVENT_CPU_STREAMS: usize = 64;
const EVENT_CPU_EVENTS: u32 = 2_000;
const EVENT_CPU_BYTES: usize = 64;

/// Scenario parameters, recorded in the report so results stay interpretable.
pub(crate) fn scenario_report() -> Value {
    json!({
        "unary": {"concurrency": UNARY_CONCURRENCY, "requests": UNARY_TOTAL, "response_bytes": 512},
        "sse": {"concurrency": SSE_CONCURRENCY, "streams": SSE_STREAMS, "events": SSE_EVENTS, "interval_us": SSE_INTERVAL_US},
        "throughput": {"concurrency": THROUGHPUT_CONCURRENCY, "streams": THROUGHPUT_STREAMS, "events": THROUGHPUT_EVENTS, "event_bytes": THROUGHPUT_EVENT_BYTES},
        "event_cpu": {"concurrency": EVENT_CPU_CONCURRENCY, "streams": EVENT_CPU_STREAMS, "events": EVENT_CPU_EVENTS, "event_bytes": EVENT_CPU_BYTES},
        "idle": {"streams": IDLE_STREAMS},
        "cancel": {"rounds": CANCEL_ROUNDS},
        "upload": {"concurrency": UPLOAD_CONCURRENCY, "requests": UPLOAD_REQUESTS, "body_bytes": UPLOAD_BYTES},
        "agent_upload": {"concurrency": UPLOAD_CONCURRENCY, "requests": AGENT_UPLOAD_REQUESTS, "body_bytes": agent_body().len()},
        "paced": paced::scenario_report(),
        "stress": stress::scenario_report(),
    })
}

/// The direct baseline of one path: what a client sees without a gateway.
pub(crate) struct Direct {
    pub(crate) latency: Latency,
    pub(crate) mb_s: f64,
    upload: (Vec<u64>, usize),
    /// Measured on the path without the paced scenario only, so the extra
    /// uploads cannot disturb the paced tail latencies.
    agent_upload: Option<(Vec<u64>, usize)>,
    paced: Option<Paced>,
    /// Measured with the paced scenario, on the production path.
    stress: Option<(Cold, Stress)>,
}

impl Direct {
    pub(crate) async fn measure(target: &Target, paced: bool) -> Self {
        let latency = Latency::measure(target).await;
        let (mb_s, _) = throughput(target).await;
        let upload = upload(target, upload_body(), UPLOAD_REQUESTS).await;
        let agent_upload = if paced {
            None
        } else {
            Some(upload_agent(target).await)
        };
        let (paced, stress) = if paced {
            (
                Some(Paced::measure(target, || {}).await),
                Some((
                    Cold::measure(target).await,
                    Stress::measure(target, || 0.0).await,
                )),
            )
        } else {
            (None, None)
        };
        Self {
            latency,
            mb_s,
            upload,
            agent_upload,
            paced,
            stress,
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
    /// The upload scenarios even with `per_request_only`: for variants whose
    /// cost grows with the request body, such as cache diagnostics.
    pub(crate) uploads: bool,
    /// CPU per streamed event even with `per_request_only`: for variants
    /// whose cost is per event, such as another API's usage scanner.
    pub(crate) event_cpu: bool,
}

/// Measures `process` against the `direct` baseline, adding `prefix`ed metrics.
///
/// Scenario order matters for memory metrics: idle streams are measured before
/// the throughput, upload, and paced scenarios, whose freed buffers the idle
/// streams would otherwise reuse (hiding their memory) or not, depending on
/// buffer sizes elsewhere.
pub(crate) async fn measure(
    direct: &Direct,
    process: &GatewayProcess,
    prefix: &str,
    scope: Scope,
    metrics: &mut BTreeMap<String, f64>,
) {
    // First, while the gateway's upstream connections are still cold.
    if let (true, Some((cold, _))) = (scope.paced && !scope.per_request_only, &direct.stress) {
        measure_cold(cold, process, prefix, metrics).await;
    }
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
    if !scope.per_request_only || scope.event_cpu {
        let cpu_before = process.cpu_secs();
        let streams = phase(
            format!("{prefix}cpu-per-event"),
            scenarios::sse(
                process.target(),
                EVENT_CPU_CONCURRENCY,
                EVENT_CPU_STREAMS,
                EVENT_CPU_EVENTS,
                EVENT_CPU_BYTES,
                0,
            ),
        )
        .await;
        let cpu_after = process.cpu_secs();
        let events: usize = streams.iter().map(|s| s.event_delays_us.len()).sum();
        // `ps` reports CPU time in 10 ms steps; 128,000 events make that < 0.1 µs per event.
        put(
            metrics,
            prefix,
            "sse_cpu_us_per_event",
            (cpu_after - cpu_before) * 1e6 / events.max(1) as f64,
        );
    }
    if scope.per_request_only {
        if scope.uploads {
            measure_uploads(direct, process, prefix, metrics).await;
        }
        return;
    }
    if scope.streams_sse {
        phase(
            format!("{prefix}idle+cancel"),
            measure_idle_and_cancel(process, prefix, metrics),
        )
        .await;
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

    measure_uploads(direct, process, prefix, metrics).await;

    if let (true, Some(direct_paced)) = (scope.paced, &direct.paced) {
        measure_paced(direct_paced, process, prefix, metrics).await;
    }

    // Stalled streams would make a buffering gateway hold whole responses.
    if let (true, true, Some((_, stress))) = (scope.paced, scope.streams_sse, &direct.stress) {
        measure_stress(stress, process, prefix, metrics).await;
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

/// Sorted latencies of successful requests, and the number that failed.
fn successes(samples: Vec<Option<u64>>) -> (Vec<u64>, usize) {
    let total = samples.len();
    let ok = sorted(samples.into_iter().flatten().collect());
    let failed = total - ok.len();
    (ok, failed)
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
