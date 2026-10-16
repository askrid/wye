//! `wye-bench load --scenario capacity`: how many token-paced events one
//! gateway core relays before its added delay grows.
//!
//! The gateway runs with one Tokio worker thread (`TOKIO_WORKER_THREADS=1`,
//! set for the child only; `WYE_BENCH_GATEWAY_THREADS` changes the count),
//! on the TLS + HTTP/2 path. Each step runs a batch
//! of streams at 50 events/s of 100 bytes, first directly against the mock
//! and then through the gateway, and reports the events per second relayed,
//! the gateway's CPU in cores, and the p99 event delay the gateway adds over
//! the direct run at the same load. Steps grow until streams fail, the
//! gateway's core is saturated, or the added p99 passes ten times
//! [`MAX_ADDED_P99_US`]; the largest step within that bound, without
//! failures or saturation, is the sustainable capacity.

use std::time::{Duration, Instant};

use crate::{client::Target, gateway_process::GatewayProcess, scenarios, stats};

/// Stream counts tried, in order.
const STEPS: [usize; 10] = [
    500, 1_000, 1_500, 2_000, 3_000, 4_000, 6_000, 8_000, 12_000, 16_000,
];
const EVENTS: u32 = 100;
const SIZE: usize = 100;
const INTERVAL_US: u64 = 20_000;
const RAMP: Duration = Duration::from_millis(2_000);
/// Added p99 event delay accepted at a sustainable load: well under a
/// typical inter-token gap of 20 ms.
pub const MAX_ADDED_P99_US: f64 = 10_000.0;
/// Share of the worker threads' cores treated as saturated.
const SATURATED_SHARE: f64 = 0.95;

struct Step {
    streams: usize,
    events_per_sec: f64,
    cores: f64,
    cpu_us_per_event: f64,
    direct_p99_us: f64,
    added_p99_us: f64,
    failed: usize,
}

async fn batch(target: &Target, streams: usize) -> (Vec<u64>, usize, usize, f64) {
    let start = Instant::now();
    let results = scenarios::paced(target, streams, RAMP, EVENTS, SIZE, INTERVAL_US).await;
    let wall = start.elapsed().as_secs_f64();
    let failed = results.iter().filter(|s| s.failed).count();
    let delays: Vec<u64> = results
        .into_iter()
        .flat_map(|s| s.event_delays_us)
        .collect();
    let events = delays.len();
    (stats::sorted(delays), events, failed, wall)
}

/// Runs the steps and prints one row per step and the sustainable maximum.
pub async fn run(direct: &Target, gateway: &GatewayProcess, threads: usize) {
    // Warm up both paths' connection pools.
    batch(direct, STEPS[0]).await;
    batch(gateway.target(), STEPS[0]).await;
    eprintln!("gateway pid {} ({threads} worker threads)", gateway.pid());
    println!(
        "{:>7} {:>10} {:>7} {:>9} {:>11} {:>11} {:>7}",
        "streams", "events/s", "cores", "us/event", "direct p99", "added p99", "failed"
    );
    let mut best: Option<Step> = None;
    for streams in STEPS {
        let (direct_delays, _, direct_failed, _) = batch(direct, streams).await;
        let cpu_before = gateway.cpu_secs();
        let (delays, events, failed, wall) = batch(gateway.target(), streams).await;
        let cpu = gateway.cpu_secs() - cpu_before;
        let direct_p99_us = stats::percentile(&direct_delays, 99.0);
        let step = Step {
            streams,
            events_per_sec: events as f64 / wall,
            cores: cpu / wall,
            cpu_us_per_event: cpu * 1e6 / events.max(1) as f64,
            direct_p99_us,
            added_p99_us: stats::percentile(&delays, 99.0) - direct_p99_us,
            failed: failed + direct_failed,
        };
        println!(
            "{:>7} {:>10.0} {:>7.2} {:>9.1} {:>9.1}ms {:>9.1}ms {:>7}",
            step.streams,
            step.events_per_sec,
            step.cores,
            step.cpu_us_per_event,
            step.direct_p99_us / 1e3,
            step.added_p99_us / 1e3,
            step.failed
        );
        let healthy = step.failed == 0 && step.cores < SATURATED_SHARE * threads as f64;
        if !healthy || step.added_p99_us > 10.0 * MAX_ADDED_P99_US {
            break;
        }
        if step.added_p99_us <= MAX_ADDED_P99_US {
            best = Some(step);
        }
    }
    match best {
        Some(step) => println!(
            "sustainable with {threads} worker threads: {} streams, {:.0} events/s \
             (added p99 at most {:.0} ms)",
            step.streams,
            step.events_per_sec,
            MAX_ADDED_P99_US / 1e3
        ),
        None => println!("no step was sustainable"),
    }
}
