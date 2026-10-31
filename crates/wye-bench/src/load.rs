//! `wye-bench load`: one scenario, sustained, so a gateway can be profiled
//! (`sample <pid>` on macOS, `perf record -p <pid>` on Linux) or soaked
//! (`paced`: memory should stay flat across batches of streams), or stepped
//! up to one core's capacity (`capacity`, see [`crate::capacity`]). `events`
//! streams unpaced token-sized events, the gated CPU-per-event scenario, so
//! the cost per event can be sampled (`--wye-openai`: OpenAI Responses).

use std::time::{Duration, Instant};

use wye_testkit::{TempDir, TestCa};

use crate::{
    allocations,
    gateway_process::{Gateway, GatewayProcess},
    measure::{
        EVENT_CPU_CONCURRENCY, EVENT_CPU_EVENTS, EVENT_CPU_STREAMS, PACED_RAMP, PACED_STREAMS,
        UPLOAD_BYTES, UPLOAD_CONCURRENCY, token_streams,
    },
    mock, scenarios,
};

/// Requests per batch; batches repeat until the time is up.
const UPLOAD_BATCH: usize = 64;

/// Runs `scenario` through `gateway` for `seconds` and prints the gateway's
/// pid at the start and its cost at the end.
pub async fn load(gateway: Gateway<'_>, scenario: &str, seconds: u64) {
    let tls = match scenario {
        "tls-upload" | "paced" | "capacity" => true,
        "upload" | "events" => false,
        _ => panic!("scenario must be upload, tls-upload, paced, capacity, or events"),
    };
    let ca = TestCa::generate();
    let upstream = mock::spawn(tls.then_some(&ca)).await;
    let base_url = format!("{}://{upstream}", if tls { "https" } else { "http" });
    let dir = TempDir::new("bench-load");
    // Capacity is measured per worker thread: one by default, or
    // `WYE_BENCH_GATEWAY_THREADS` for a multi-core run.
    let threads = std::env::var("WYE_BENCH_GATEWAY_THREADS").unwrap_or_else(|_| "1".into());
    let capacity_env = [("TOKIO_WORKER_THREADS", threads.as_str())];
    let env: &[(&str, &str)] = if scenario == "capacity" {
        &capacity_env
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
    match scenario {
        "capacity" => {
            let direct = crate::client::Target::tls(upstream, ca.ca_cert());
            let threads = threads
                .parse()
                .expect("WYE_BENCH_GATEWAY_THREADS must be a number");
            crate::capacity::run(&direct, &process, threads).await;
        }
        "events" => soak_events(&process, seconds).await,
        "paced" => soak_paced(&process, seconds).await,
        _ => soak_uploads(&process, scenario, seconds).await,
    }
}

/// Batches of agent-sized uploads until `seconds` pass; prints the gateway's CPU per MB.
async fn soak_uploads(process: &GatewayProcess, scenario: &str, seconds: u64) {
    let body = mock::message_json(UPLOAD_BYTES);
    let run = || {
        scenarios::upload(
            process.target(),
            UPLOAD_CONCURRENCY,
            UPLOAD_BATCH,
            body.clone(),
        )
    };
    // Warm up connection pools before the measured window.
    run().await;
    eprintln!(
        "gateway pid {} under {scenario} for {seconds}s",
        process.pid()
    );
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let cpu_before = process.cpu_secs();
    let mut requests = 0;
    let mut failed = 0;
    while Instant::now() < deadline {
        let results = run().await;
        failed += results.iter().filter(|r| r.is_none()).count();
        requests += results.len();
    }
    let cpu = process.cpu_secs() - cpu_before;
    let mb = (requests * UPLOAD_BYTES) as f64 / 1e6;
    println!(
        "{requests} requests ({failed} failed), {mb:.0} MB, gateway CPU {:.2} ms/MB",
        cpu * 1_000.0 / mb
    );
}

/// Batches of unpaced 64-byte events (64 streams of 2,000) until `seconds`
/// pass; prints the gateway's CPU per event.
async fn soak_events(process: &GatewayProcess, seconds: u64) {
    // `WYE_BENCH_EVENT_BYTES` changes the payload, to compare event sizes.
    let bytes = std::env::var("WYE_BENCH_EVENT_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let run = || {
        scenarios::sse(
            process.target(),
            EVENT_CPU_CONCURRENCY,
            EVENT_CPU_STREAMS,
            EVENT_CPU_EVENTS,
            bytes,
            0,
        )
    };
    run().await;
    eprintln!("gateway pid {} under events for {seconds}s", process.pid());
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let cpu_before = process.cpu_secs();
    let (mut events, mut failed) = (0usize, 0usize);
    while Instant::now() < deadline {
        let streams = run().await;
        failed += scenarios::count_failed(&streams);
        events += scenarios::count_events(&streams);
    }
    let cpu = process.cpu_secs() - cpu_before;
    println!(
        "{events} events ({failed} streams failed), gateway CPU {:.2} us/event",
        cpu * 1e6 / events.max(1) as f64
    );
}

/// Batches of token-paced streams until `seconds` pass; prints CPU per event
/// and the gateway's resident memory after each batch, so growth shows.
async fn soak_paced(process: &GatewayProcess, seconds: u64) {
    let run = || token_streams(process.target(), PACED_STREAMS, PACED_RAMP);
    run().await;
    eprintln!("gateway pid {} under paced for {seconds}s", process.pid());
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let cpu_before = process.cpu_secs();
    let (mut events, mut failed, mut batches) = (0usize, 0usize, 0usize);
    let mut rss = Vec::new();
    let mut live = Vec::new();
    while Instant::now() < deadline {
        let streams = run().await;
        failed += scenarios::count_failed(&streams);
        events += scenarios::count_events(&streams);
        batches += 1;
        // Idle between batches: what the gateway keeps once streams end.
        tokio::time::sleep(Duration::from_millis(200)).await;
        rss.push(process.rss_kb());
        let live_kb = allocations::live_bytes(process.target())
            .await
            .map(|bytes| bytes / 1024.0);
        live.extend(live_kb);
        // One line per batch as it ends, so an interrupted multi-hour run
        // still leaves its series.
        eprintln!(
            "batch {batches} at {:.0}s: {failed} failed streams so far, idle RSS {:.0} KB, live heap {} KB",
            started.elapsed().as_secs_f64(),
            rss.last().copied().unwrap_or_default(),
            live_kb.map_or_else(|| "-".to_owned(), |kb| format!("{kb:.0}")),
        );
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
