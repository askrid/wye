//! `wye-bench load`: one scenario, sustained, so a gateway can be profiled
//! (`sample <pid>` on macOS, `perf record -p <pid>` on Linux) or soaked
//! (`paced`: memory should stay flat across batches of streams), or stepped
//! up to one core's capacity (`capacity`, see [`crate::capacity`]).

use std::time::{Duration, Instant};

use wye_testkit::TestCa;

use crate::{
    allocations,
    gateway_process::{Gateway, GatewayProcess},
    mock, scenarios,
};

const UPLOAD_BYTES: usize = 1024 * 1024;
const UPLOAD_CONCURRENCY: usize = 16;
/// Requests per batch; batches repeat until the time is up.
const UPLOAD_BATCH: usize = 64;

/// Token-paced streams per batch in the `paced` scenario (TLS + HTTP/2):
/// 50 events/s of 100 bytes for 2 s, like the gated paced scenario.
const PACED_STREAMS: usize = 2_000;
const PACED_EVENTS: u32 = 100;
const PACED_INTERVAL_US: u64 = 20_000;
const PACED_RAMP: Duration = Duration::from_millis(2_000);

/// Runs `scenario` through `gateway` for `seconds` and prints the gateway's
/// pid at the start and its cost at the end.
pub async fn load(gateway: Gateway<'_>, scenario: &str, seconds: u64) {
    let tls = match scenario {
        "tls-upload" | "paced" | "capacity" => true,
        "upload" => false,
        _ => panic!("scenario must be upload, tls-upload, paced, or capacity"),
    };
    let ca = TestCa::generate();
    let upstream = mock::spawn(tls.then_some(&ca)).await;
    let base_url = format!("{}://{upstream}", if tls { "https" } else { "http" });
    let dir = std::env::temp_dir().join(format!("wye-bench-load-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Capacity is measured per core: one Tokio worker thread for the gateway.
    let env: &[(&str, &str)] = if scenario == "capacity" {
        &[("TOKIO_WORKER_THREADS", "1")]
    } else {
        &[]
    };
    let process = GatewayProcess::start_with_env(
        gateway,
        &base_url,
        tls.then_some(ca.ca_pem()),
        &dir,
        scenario,
        env,
    )
    .await;
    if scenario == "capacity" {
        let direct = crate::client::Target::tls(upstream, ca.ca_cert());
        crate::capacity::run(&direct, &process).await;
        drop(process);
        let _ = std::fs::remove_dir_all(dir);
        return;
    }
    if scenario == "paced" {
        soak_paced(&process, seconds).await;
        drop(process);
        let _ = std::fs::remove_dir_all(dir);
        return;
    }
    let body = mock::padded_json(
        r#"{"model":"bench","max_tokens":1,"messages":[{"role":"user","content":""#,
        r#""}]}"#,
        UPLOAD_BYTES,
    );
    // Warm up connection pools before the measured window.
    scenarios::upload(
        process.target(),
        UPLOAD_CONCURRENCY,
        UPLOAD_BATCH,
        body.clone(),
    )
    .await;
    eprintln!(
        "gateway pid {} under {scenario} for {seconds}s",
        process.pid()
    );
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let cpu_before = process.cpu_secs();
    let mut requests = 0;
    let mut failed = 0;
    while Instant::now() < deadline {
        let results = scenarios::upload(
            process.target(),
            UPLOAD_CONCURRENCY,
            UPLOAD_BATCH,
            body.clone(),
        )
        .await;
        failed += results.iter().filter(|r| r.is_none()).count();
        requests += results.len();
    }
    let cpu = process.cpu_secs() - cpu_before;
    let mb = (requests * UPLOAD_BYTES) as f64 / 1e6;
    println!(
        "{requests} requests ({failed} failed), {mb:.0} MB, gateway CPU {:.2} ms/MB",
        cpu * 1_000.0 / mb
    );
    drop(process);
    let _ = std::fs::remove_dir_all(dir);
}

/// Batches of token-paced streams until `seconds` pass; prints CPU per event
/// and the gateway's resident memory after each batch, so growth shows.
async fn soak_paced(process: &GatewayProcess, seconds: u64) {
    let run = || {
        scenarios::paced(
            process.target(),
            PACED_STREAMS,
            PACED_RAMP,
            PACED_EVENTS,
            100,
            PACED_INTERVAL_US,
        )
    };
    run().await;
    eprintln!("gateway pid {} under paced for {seconds}s", process.pid());
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let cpu_before = process.cpu_secs();
    let (mut events, mut failed, mut batches) = (0usize, 0usize, 0usize);
    let mut rss = Vec::new();
    let mut live = Vec::new();
    while Instant::now() < deadline {
        let streams = run().await;
        failed += streams.iter().filter(|s| s.failed).count();
        events += streams
            .iter()
            .map(|s| s.event_delays_us.len())
            .sum::<usize>();
        batches += 1;
        // Idle between batches: what the gateway keeps once streams end.
        tokio::time::sleep(Duration::from_millis(200)).await;
        rss.push(process.rss_kb());
        if let Some(bytes) = allocations::live_bytes(process.target()).await {
            live.push(bytes / 1024.0);
        }
    }
    let cpu = process.cpu_secs() - cpu_before;
    let first = rss.first().copied().unwrap_or_default();
    let last = rss.last().copied().unwrap_or_default();
    let max = rss.iter().copied().fold(0.0, f64::max);
    println!(
        "{batches} batches, {events} events ({failed} failed streams), CPU {:.1} us/event; \
         idle RSS after batches: first {:.0} KB, last {:.0} KB, max {:.0} KB",
        cpu * 1e6 / events.max(1) as f64,
        first,
        last,
        max
    );
    let series = |v: &[f64]| {
        v.iter()
            .map(|x| format!("{x:.0}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    println!("idle RSS KB per batch: {}", series(&rss));
    if !live.is_empty() {
        println!(
            "live heap KB per batch (alloc-stats build): {}",
            series(&live)
        );
    }
}
