//! Measuring one gateway process against the direct baseline of one upstream path.

use std::time::Duration;

use serde_json::{Value, json};

mod paced;
mod stress;
mod upload;

// The gated scenarios' sizes, for the load and allocation runs to reproduce.
pub(crate) use paced::{PACED_RAMP, PACED_STREAMS, token_streams};
use upload::{
    AGENT_UPLOAD_REQUESTS, UPLOAD_REQUESTS, agent_body, measure_uploads, upload, upload_body,
};
pub(crate) use upload::{UPLOAD_BYTES, UPLOAD_CONCURRENCY};

use paced::{Paced, measure_paced};
use stress::{Cold, Stress, measure_cold, measure_stress};

use crate::{
    client::Target,
    gateway_process::GatewayProcess,
    scenarios::{self, Stream},
    stats::{Out, PERCENTILES, event_delays, percentile, phase, sorted},
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
pub(crate) const EVENT_CPU_CONCURRENCY: usize = 32;
pub(crate) const EVENT_CPU_STREAMS: usize = 64;
pub(crate) const EVENT_CPU_EVENTS: u32 = 2_000;
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
    upload: Vec<u64>,
    /// Measured on the path without the paced scenario only, so the extra
    /// uploads cannot disturb the paced tail latencies.
    agent_upload: Option<Vec<u64>>,
    paced: Option<Paced>,
    /// Measured with the paced scenario, on the production path.
    stress: Option<(Cold, Stress)>,
}

impl Direct {
    pub(crate) async fn measure(target: &Target, paced: bool) -> Self {
        let latency = Latency::measure(target).await;
        let (mb_s, _, throughput_failed) = throughput(target).await;
        let (uploads, uploads_failed) = upload(target, upload_body(), UPLOAD_REQUESTS).await;
        let (agent_upload, agent_failed) = if paced {
            (None, 0)
        } else {
            let (samples, failed) = upload(target, agent_body(), AGENT_UPLOAD_REQUESTS).await;
            (Some(samples), failed)
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
        let failed = latency.unary_failed
            + latency.sse_failed
            + throughput_failed
            + uploads_failed
            + agent_failed
            + paced.as_ref().map_or(0, |p| p.failed_streams)
            + stress
                .as_ref()
                .map_or(0, |(cold, s)| cold.1 + s.mixed_failed + s.stalled_failed);
        // Nothing gates the baseline, and it skews every `added_` metric of this path.
        if failed > 0 {
            eprintln!("warning: {failed} requests or streams failed on the direct baseline");
        }
        Self {
            latency,
            mb_s,
            upload: uploads,
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

/// Measures `process` against the `direct` baseline, adding metrics to `out`.
///
/// Scenario order matters for memory metrics: idle streams are measured before
/// the throughput, upload, and paced scenarios, whose freed buffers the idle
/// streams would otherwise reuse (hiding their memory) or not, depending on
/// buffer sizes elsewhere.
pub(crate) async fn measure(
    direct: &Direct,
    process: &GatewayProcess,
    scope: Scope,
    out: &mut Out<'_>,
) {
    let prefix = out.prefix;
    // First, while the gateway's upstream connections are still cold.
    if !scope.per_request_only
        && let Some((cold, _)) = &direct.stress
    {
        measure_cold(cold, process, out).await;
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
    out.put(
        "unary_cpu_us_per_request",
        (cpu_after - cpu_before) * 1e6 / (CPU_PER_REQUEST_TOTAL - failed).max(1) as f64,
    );

    let base = &direct.latency;
    // Failures in the CPU phases count too: for some variants they run last.
    out.put("unary_failed_requests", (via.unary_failed + failed) as f64);
    let mut sse_failed = via.sse_failed;
    for (label, p) in PERCENTILES {
        let added = |via: &[u64], direct: &[u64]| percentile(via, p) - percentile(direct, p);
        out.put(&format!("via_unary_{label}_us"), percentile(&via.unary, p));
        out.put(
            &format!("unary_added_{label}_us"),
            added(&via.unary, &base.unary),
        );
        out.put(
            &format!("sse_ttfb_added_{label}_us"),
            added(&via.ttfb, &base.ttfb),
        );
        out.put(
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
        sse_failed += scenarios::count_failed(&streams);
        let events = scenarios::count_events(&streams);
        // CPU time comes in 10 ms steps; 128,000 events make that < 0.1 µs per event.
        out.put(
            "sse_cpu_us_per_event",
            (cpu_after - cpu_before) * 1e6 / events.max(1) as f64,
        );
    }
    out.put("sse_failed_streams", sse_failed as f64);
    if scope.per_request_only {
        if scope.uploads {
            measure_uploads(direct, process, out).await;
        }
        return;
    }
    if scope.streams_sse {
        phase(
            format!("{prefix}idle+cancel"),
            measure_idle_and_cancel(process, out),
        )
        .await;
    }

    let cpu_before = process.cpu_secs();
    let (mb_s, mb, failed) =
        phase(format!("{prefix}throughput"), throughput(process.target())).await;
    let cpu_after = process.cpu_secs();
    out.add("sse_failed_streams", failed as f64);
    out.put("via_throughput_mb_s", mb_s);
    // CPU time comes in 10 ms steps; the scenario streams enough data to
    // make that negligible.
    out.put(
        "throughput_cpu_ms_per_mb",
        (cpu_after - cpu_before) * 1_000.0 / mb,
    );

    measure_uploads(direct, process, out).await;

    if let Some(direct_paced) = &direct.paced {
        measure_paced(direct_paced, process, out).await;
    }

    // Stalled streams would make a buffering gateway hold whole responses.
    if scope.streams_sse
        && let Some((_, stress)) = &direct.stress
    {
        measure_stress(stress, process, out).await;
    }

    out.put("rss_after_load_kb", process.rss_kb());
}

/// Memory per open idle stream, and upstream release after a client disconnect.
async fn measure_idle_and_cancel(process: &GatewayProcess, out: &mut Out<'_>) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rss_before = process.rss_kb();
    let held = scenarios::open_idle_streams(process.target(), IDLE_STREAMS, None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rss_open = process.rss_kb();
    drop(held);
    out.put(
        "idle_rss_kb_per_stream",
        (rss_open - rss_before) / IDLE_STREAMS as f64,
    );
    out.put("rss_idle_kb", rss_before);

    let cancel = sorted(scenarios::cancel_latency(process.target(), CANCEL_ROUNDS).await);
    out.put("cancel_upstream_release_p50_us", percentile(&cancel, 50.0));
    out.put("cancel_upstream_release_max_us", percentile(&cancel, 100.0));
}

/// Sorted latency samples of the unary and SSE scenarios, of the requests
/// and streams that succeeded, and the number that failed.
pub(crate) struct Latency {
    pub(crate) unary: Vec<u64>,
    pub(crate) unary_failed: usize,
    pub(crate) ttfb: Vec<u64>,
    pub(crate) event_delay: Vec<u64>,
    pub(crate) sse_failed: usize,
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
            // A failed stream's TTFB of 0 would pull the percentiles down.
            ttfb: sorted(
                streams
                    .iter()
                    .filter(|s| !s.failed)
                    .map(|s| s.ttfb_us)
                    .collect(),
            ),
            sse_failed: scenarios::count_failed(&streams),
            event_delay: event_delays(streams),
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

/// Returns (MB/s, MB streamed, failed streams) for large back-to-back events.
async fn throughput(target: &Target) -> (f64, f64, usize) {
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
    let failed = scenarios::count_failed(&streams);
    (mb / start.elapsed().as_secs_f64(), mb, failed)
}
