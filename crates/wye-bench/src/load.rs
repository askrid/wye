//! `wye-bench load`: one scenario, sustained, so a gateway can be profiled
//! (`sample <pid>` on macOS, `perf record -p <pid>` on Linux).

use std::time::{Duration, Instant};

use wye_testkit::TestCa;

use crate::{
    gateway_process::{Gateway, GatewayProcess},
    mock, scenarios,
};

const UPLOAD_BYTES: usize = 1024 * 1024;
const UPLOAD_CONCURRENCY: usize = 16;
/// Requests per batch; batches repeat until the time is up.
const UPLOAD_BATCH: usize = 64;

/// Runs `scenario` through `gateway` for `seconds` and prints the gateway's
/// pid at the start and its CPU per uploaded MB at the end.
pub async fn load(gateway: Gateway<'_>, scenario: &str, seconds: u64) {
    let tls = match scenario {
        "tls-upload" => true,
        "upload" => false,
        _ => panic!("scenario must be upload or tls-upload"),
    };
    let ca = TestCa::generate();
    let upstream = mock::spawn(tls.then_some(&ca)).await;
    let base_url = format!("{}://{upstream}", if tls { "https" } else { "http" });
    let dir = std::env::temp_dir().join(format!("wye-bench-load-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let process = GatewayProcess::start(
        gateway,
        &base_url,
        tls.then_some(ca.ca_pem()),
        &dir,
        scenario,
    )
    .await;
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
