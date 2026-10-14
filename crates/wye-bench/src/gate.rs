//! Regression gate: `compare` and `adopt-new`.

use serde_json::Value;

/// Gated metrics as (name, relative tolerance, absolute tolerance). Lower is
/// better for all of them. Tolerances come from the spread of repeated runs of
/// unchanged code on the baseline machine.
///
/// Reported but not gated: p99 time to first byte (swung from -521 to 1694 µs
/// between identical runs; in the paced scenario it is dominated by connection
/// bursts), the TLS unary p99, and upload p99. The paced scenario runs only on
/// the TLS + HTTP/2 path. Also reported but not yet gated: the agent-shaped
/// upload, the model-permissions variant, and the cache-diagnostics variant
/// (`agent_upload_*`, `models_*`, `diag_*`, `alloc_*agent_upload*`,
/// `alloc_models_*`, `alloc_diag_*`; plain path only), until their baseline is
/// reviewed.
const GATES: &[(&str, f64, f64)] = &[
    // Plain HTTP/1.1 upstream.
    ("unary_added_p50_us", 0.20, 40.0),
    ("unary_added_p99_us", 0.50, 400.0),
    ("sse_ttfb_added_p50_us", 0.40, 100.0),
    ("sse_event_added_p50_us", 0.20, 40.0),
    ("sse_event_added_p99_us", 0.50, 150.0),
    ("throughput_cpu_ms_per_mb", 0.12, 0.05),
    ("idle_rss_kb_per_stream", 0.10, 2.0),
    // TLS + HTTP/2 upstream, the production path; noisier on this machine.
    ("tls_unary_added_p50_us", 0.50, 200.0),
    ("tls_sse_ttfb_added_p50_us", 1.50, 200.0),
    ("tls_sse_event_added_p50_us", 0.50, 100.0),
    ("tls_sse_event_added_p99_us", 0.50, 300.0),
    ("tls_throughput_cpu_ms_per_mb", 0.25, 0.05),
    ("tls_idle_rss_kb_per_stream", 0.15, 2.0),
    // CPU per request, and Wye with gateway credentials (plain path): the
    // credential check costs per request, so these catch auth regressions.
    ("unary_cpu_us_per_request", 0.20, 10.0),
    ("tls_unary_cpu_us_per_request", 0.25, 10.0),
    ("auth_unary_cpu_us_per_request", 0.20, 10.0),
    ("auth_unary_added_p50_us", 0.20, 40.0),
    ("auth_sse_ttfb_added_p50_us", 0.40, 100.0),
    ("auth_unary_failed_requests", 0.0, 0.0),
    // Heap allocations (alloc-stats build). Nearly deterministic, so the
    // limits catch one extra allocation per request, stream, or event.
    ("alloc_unary_per_request", 0.02, 0.5),
    ("alloc_unary_bytes_per_request", 0.02, 256.0),
    ("alloc_auth_unary_per_request", 0.02, 0.5),
    ("alloc_sse_per_stream", 0.02, 0.5),
    ("alloc_sse_per_event", 0.0, 0.05),
    ("alloc_idle_live_bytes_per_stream", 0.05, 1024.0),
    // Idle streams after an agent-sized (512 KB) request, and on the TLS +
    // HTTP/2 upstream: the production shape of per-stream memory.
    (
        "alloc_idle_after_upload_live_bytes_per_stream",
        0.10,
        2048.0,
    ),
    ("alloc_tls_idle_live_bytes_per_stream", 0.05, 1024.0),
    (
        "alloc_tls_idle_after_upload_live_bytes_per_stream",
        0.10,
        2048.0,
    ),
    // Uploads vary about 5% with how the body arrives in chunks.
    ("alloc_upload_per_request", 0.15, 2.0),
    ("alloc_upload_bytes_per_request", 0.15, 0.0),
    // Realistic workloads: agent-sized uploads and token-paced streams at scale.
    // These load the whole machine, so latency tolerances are wide; CPU per
    // unit of work and memory per stream are the precise signals.
    ("upload_added_p50_us", 0.30, 1_000.0),
    ("upload_cpu_ms_per_mb", 0.20, 0.1),
    ("tls_upload_added_p50_us", 0.50, 1_000.0),
    ("tls_upload_cpu_ms_per_mb", 0.25, 0.1),
    ("tls_paced_event_added_p50_us", 0.50, 1_000.0),
    ("tls_paced_event_added_p99_us", 0.50, 2_000.0),
    ("tls_paced_cpu_us_per_event", 0.25, 2.0),
    ("tls_paced_rss_kb_per_stream", 0.15, 1.0),
    // Any failed request or dropped stream is a regression.
    ("unary_failed_requests", 0.0, 0.0),
    ("upload_failed_requests", 0.0, 0.0),
    ("tls_unary_failed_requests", 0.0, 0.0),
    ("tls_upload_failed_requests", 0.0, 0.0),
    ("tls_paced_failed_streams", 0.0, 0.0),
    // Guard the property, not the microseconds: a release that waits for the
    // next upstream write would take seconds.
    ("cancel_upstream_release_max_us", 1.0, 20_000.0),
    ("tls_cancel_upstream_release_max_us", 1.0, 20_000.0),
];

/// Prints a comparison table; returns false if any gated metric regressed or is missing.
pub fn compare(baseline: &str, current: &str) -> bool {
    let (base, cur) = (load(baseline), load(current));
    if base["machine"] != cur["machine"] {
        eprintln!(
            "warning: baseline was recorded on a different machine; comparison is unreliable"
        );
    }
    println!(
        "{:36} {:>10} {:>10} {:>10}  result",
        "metric", "baseline", "current", "limit"
    );
    let mut ok = true;
    for &(name, rel, abs) in GATES {
        let (Some(b), Some(c)) = (
            base["metrics"][name].as_f64(),
            cur["metrics"][name].as_f64(),
        ) else {
            println!("{name:36} missing");
            ok = false;
            continue;
        };
        // Added-latency metrics can be near zero or negative (noise); the absolute floor absorbs that.
        let limit = b.max(0.0) * (1.0 + rel) + abs;
        let pass = c <= limit;
        ok &= pass;
        println!(
            "{name:36} {b:>10.2} {c:>10.2} {limit:>10.2}  {}",
            if pass { "ok" } else { "REGRESSED" }
        );
    }
    ok
}

/// Copies metrics that exist in `current` but not in `baseline` into the baseline.
///
/// Used when a metric is added: existing baseline values stay untouched, so
/// adding a metric never loosens an existing gate.
pub fn adopt_new(baseline: &str, current: &str) {
    let mut base = load(baseline);
    let cur = load(current);
    let metrics = base["metrics"].as_object_mut().unwrap();
    for (name, value) in cur["metrics"].as_object().unwrap() {
        if !metrics.contains_key(name) {
            println!("adopted {name} = {value}");
            metrics.insert(name.clone(), value.clone());
        }
    }
    std::fs::write(
        baseline,
        serde_json::to_string_pretty(&base).unwrap() + "\n",
    )
    .unwrap();
}

fn load(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}")))
        .unwrap()
}
