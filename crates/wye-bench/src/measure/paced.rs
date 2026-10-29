//! Token-paced streams at scale: thousands of concurrent streams at model
//! output pace, where per-event cost and memory per stream dominate.

use std::{collections::BTreeMap, time::Duration};

use crate::{
    client::Target,
    gateway_process::GatewayProcess,
    scenarios,
    stats::{PERCENTILES, percentile, phase, put, sorted},
};

/// Token-paced streams at scale: 50 events/s of ~100 bytes, like model output.
pub(crate) const PACED_STREAMS: usize = 2_000;
pub(crate) const PACED_EVENTS: u32 = 100;
pub(crate) const PACED_INTERVAL_US: u64 = 20_000;
pub(crate) const PACED_EVENT_BYTES: usize = 100;
/// Stream starts are spread over this time (one per millisecond): macOS caps
/// the listen backlog at 128, and over HTTP/1.1 every stream needs its own
/// upstream connection, so faster bursts would measure SYN retries.
pub(crate) const PACED_RAMP: Duration = Duration::from_millis(2_000);

/// The parameters, recorded in the report.
pub(super) fn scenario_report() -> serde_json::Value {
    serde_json::json!({"streams": PACED_STREAMS, "events": PACED_EVENTS, "interval_us": PACED_INTERVAL_US, "event_bytes": PACED_EVENT_BYTES, "ramp_ms": PACED_RAMP.as_millis() as u64})
}

/// Sorted samples of the token-paced scenario.
pub(crate) struct Paced {
    ttfb: Vec<u64>,
    event_delay: Vec<u64>,
    failed_streams: usize,
}

impl Paced {
    /// Runs all streams concurrently; `while_loaded` runs once every stream is open.
    pub(super) async fn measure(target: &Target, while_loaded: impl FnOnce()) -> Self {
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

/// Runs the paced scenario through `process` and adds what it costs over `direct`.
pub(super) async fn measure_paced(
    direct: &Paced,
    process: &GatewayProcess,
    prefix: &str,
    metrics: &mut BTreeMap<String, f64>,
) {
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
        let added = percentile(&paced.event_delay, p) - percentile(&direct.event_delay, p);
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
        percentile(&paced.ttfb, 99.0) - percentile(&direct.ttfb, 99.0),
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
