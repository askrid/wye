//! Agent traffic under stress, on the production path (TLS + HTTP/2): the
//! first requests on cold connections, token streams while large contexts
//! upload, and token streams beside clients that stall. Each runs directly
//! against the mock and through the gateway under the same load, and the
//! gateway's metrics are the difference.

use std::{collections::BTreeMap, time::Duration};

use super::{paced::token_streams, successes, upload::upload_body};
use crate::{
    client::Target,
    gateway_process::GatewayProcess,
    scenarios,
    stats::{PERCENTILES, event_delays, percentile, phase, put},
};

/// Token-paced streams in the mixed and stalled scenarios.
const STREAMS: usize = 1_000;
const RAMP: Duration = Duration::from_millis(1_000);
/// 1 MiB uploads kept in flight while the mixed scenario's streams run.
const UPLOAD_CONCURRENCY: usize = 8;
/// Clients that stop reading mid-stream.
const STALLED: usize = 200;
/// First requests on new connections, all at once.
const COLD: usize = 32;

/// The parameters, recorded in the report.
pub(super) fn scenario_report() -> serde_json::Value {
    serde_json::json!({
        "cold": {"requests": COLD},
        "mixed": {"streams": STREAMS, "ramp_ms": RAMP.as_millis() as u64, "upload_concurrency": UPLOAD_CONCURRENCY},
        "stalled": {"stalled_streams": STALLED, "streams": STREAMS},
    })
}

/// Samples of one run of the stress scenarios.
pub(crate) struct Stress {
    mixed_events: Vec<u64>,
    mixed_uploads: Vec<u64>,
    mixed_failed: usize,
    stalled_events: Vec<u64>,
    stalled_failed: usize,
    /// Resident memory added per stalled stream, in KB.
    stalled_rss_kb: f64,
}

/// Latencies of the first requests on new connections, and failures.
pub(crate) struct Cold(Vec<u64>, usize);

impl Cold {
    /// Must be the first traffic a gateway sees, while its upstream
    /// connections are still closed.
    pub(crate) async fn measure(target: &Target) -> Self {
        let (samples, failed) = successes(scenarios::cold_unary(target, COLD).await);
        Self(samples, failed)
    }
}

impl Stress {
    /// `rss` reads the measured process's resident memory; direct runs pass
    /// one that reads nothing.
    pub(crate) async fn measure(target: &Target, rss: impl Fn() -> f64) -> Self {
        let streams = {
            let target = target.clone();
            tokio::spawn(async move { token_streams(&target, STREAMS, RAMP).await })
        };
        let body = upload_body();
        let mut uploads = Vec::new();
        while !streams.is_finished() {
            uploads.extend(
                scenarios::upload(target, UPLOAD_CONCURRENCY, UPLOAD_CONCURRENCY, body.clone())
                    .await,
            );
        }
        let streams = streams.await.unwrap();
        let (mixed_uploads, uploads_failed) = successes(uploads);
        let mixed_failed = uploads_failed + streams.iter().filter(|s| s.failed).count();
        let mixed_events = event_delays(streams);

        tokio::time::sleep(Duration::from_millis(500)).await;
        let rss_before = rss();
        let held = scenarios::stalled_streams(target, STALLED).await;
        // Let every stalled stream fill its buffers and flow-control windows.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let stalled_rss_kb = (rss() - rss_before) / STALLED as f64;
        let streams = token_streams(target, STREAMS, RAMP).await;
        let stalled_failed = STALLED - held.len() + streams.iter().filter(|s| s.failed).count();
        drop(held);
        let stalled_events = event_delays(streams);

        Self {
            mixed_events,
            mixed_uploads,
            mixed_failed,
            stalled_events,
            stalled_failed,
            stalled_rss_kb,
        }
    }
}

/// Cold-connection latency through a freshly started `process`, over `direct`.
pub(super) async fn measure_cold(
    direct: &Cold,
    process: &GatewayProcess,
    prefix: &str,
    metrics: &mut BTreeMap<String, f64>,
) {
    let via = phase(format!("{prefix}cold"), Cold::measure(process.target())).await;
    for (name, p) in PERCENTILES {
        put(
            metrics,
            prefix,
            &format!("cold_unary_added_{name}_us"),
            percentile(&via.0, p) - percentile(&direct.0, p),
        );
    }
    put(metrics, prefix, "cold_failed_requests", via.1 as f64);
}

/// Runs the stress scenarios through `process` and adds what it costs over `direct`.
pub(super) async fn measure_stress(
    direct: &Stress,
    process: &GatewayProcess,
    prefix: &str,
    metrics: &mut BTreeMap<String, f64>,
) {
    let via = phase(
        format!("{prefix}stress"),
        Stress::measure(process.target(), || process.rss_kb()),
    )
    .await;
    let added = |via: &[u64], direct: &[u64], p| percentile(via, p) - percentile(direct, p);
    for (name, p) in PERCENTILES {
        put(
            metrics,
            prefix,
            &format!("mixed_event_added_{name}_us"),
            added(&via.mixed_events, &direct.mixed_events, p),
        );
    }
    put(
        metrics,
        prefix,
        "mixed_upload_added_p50_us",
        added(&via.mixed_uploads, &direct.mixed_uploads, 50.0),
    );
    put(
        metrics,
        prefix,
        "stalled_event_added_p99_us",
        added(&via.stalled_events, &direct.stalled_events, 99.0),
    );
    put(
        metrics,
        prefix,
        "stalled_rss_kb_per_stream",
        via.stalled_rss_kb,
    );
    put(
        metrics,
        prefix,
        "stress_failed_requests",
        (via.mixed_failed + via.stalled_failed) as f64,
    );
}
