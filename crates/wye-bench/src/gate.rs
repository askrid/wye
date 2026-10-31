//! Regression gate: `compare`, and `median` to merge alternating rounds.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::stats;

/// The most a `*_release_max_us` metric may read, whatever the reference.
const RELEASE_CEILING_US: f64 = 1_000_000.0;

/// Gated metrics as (name, relative tolerance, absolute tolerance). Lower is
/// better for all of them. Tolerances come from the spread of repeated runs of
/// unchanged code on the development laptop; the SSE time-to-first-byte and
/// per-event p50s, cold connections, and the stalled-client p99 from eight A/A
/// comparisons of the alternating-rounds gate (`docs/benchmarks.md`).
///
/// Reported but not gated: p99 time to first byte (swung from -521 to 1694 µs
/// between identical runs; in the paced scenario it is dominated by connection
/// bursts), the TLS unary p99, and upload p99. The paced scenario runs only on
/// the TLS + HTTP/2 path. Upload allocation counts for the agent-shaped body
/// are reported but not gated, since they swing about 20% with read timing,
/// except on the `failover_` variant: there every kept chunk is an
/// allocation, which makes the count steady (99.3 and 99.6 in two runs).
const GATES: &[(&str, f64, f64)] = &[
    // Plain HTTP/1.1 upstream.
    ("unary_added_p50_us", 0.20, 40.0),
    ("unary_added_p99_us", 0.50, 400.0),
    ("sse_ttfb_added_p50_us", 0.40, 300.0),
    ("sse_event_added_p50_us", 0.20, 160.0),
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
    // CPU per request, and Wye with gateway tokens (plain path): the
    // credential check costs per request, so these catch auth regressions.
    ("unary_cpu_us_per_request", 0.20, 10.0),
    ("tls_unary_cpu_us_per_request", 0.25, 10.0),
    ("auth_unary_cpu_us_per_request", 0.20, 10.0),
    ("auth_unary_added_p50_us", 0.20, 40.0),
    ("auth_sse_ttfb_added_p50_us", 0.40, 350.0),
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
    // A client that stops reading: bounded by the upstream stream window.
    ("alloc_tls_stalled_live_bytes_per_stream", 0.05, 65_536.0),
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
    // The agent-shaped upload (plain path), and the variants whose cost grows
    // with the request body: model permissions (`models_`, the repeated-key
    // check) and cache diagnostics (`diag_`, the prompt scanner).
    ("agent_upload_cpu_ms_per_mb", 0.20, 0.1),
    ("models_unary_cpu_us_per_request", 0.20, 10.0),
    ("models_unary_added_p50_us", 0.20, 40.0),
    ("models_agent_upload_cpu_ms_per_mb", 0.25, 0.1),
    ("alloc_models_unary_per_request", 0.02, 0.5),
    ("diag_unary_cpu_us_per_request", 0.20, 10.0),
    ("diag_agent_upload_cpu_ms_per_mb", 0.25, 0.1),
    ("alloc_diag_unary_per_request", 0.02, 0.5),
    ("alloc_diag_unary_bytes_per_request", 0.02, 256.0),
    ("agent_upload_failed_requests", 0.0, 0.0),
    ("models_unary_failed_requests", 0.0, 0.0),
    ("models_upload_failed_requests", 0.0, 0.0),
    ("models_agent_upload_failed_requests", 0.0, 0.0),
    ("diag_unary_failed_requests", 0.0, 0.0),
    ("diag_upload_failed_requests", 0.0, 0.0),
    ("diag_agent_upload_failed_requests", 0.0, 0.0),
    // CPU per token-sized event, unpaced (plain path): the Messages usage
    // scanner, and Wye on `[routes.openai]` with the mock framing OpenAI
    // Responses streams and JSON (the Responses scanners).
    ("sse_cpu_us_per_event", 0.25, 0.5),
    ("openai_sse_cpu_us_per_event", 0.25, 0.5),
    ("openai_unary_cpu_us_per_request", 0.20, 10.0),
    ("openai_unary_added_p50_us", 0.20, 40.0),
    ("openai_sse_ttfb_added_p50_us", 0.40, 350.0),
    ("openai_unary_failed_requests", 0.0, 0.0),
    // A Claude route to Bedrock: every request held until its model and the
    // model id rewritten (`anthropic.` prefix).
    ("bedrock_unary_cpu_us_per_request", 0.20, 10.0),
    ("bedrock_unary_added_p50_us", 0.20, 40.0),
    ("bedrock_unary_failed_requests", 0.0, 0.0),
    ("alloc_bedrock_unary_per_request", 0.02, 0.5),
    ("alloc_bedrock_unary_bytes_per_request", 0.02, 256.0),
    // A Claude route with failover: every request body kept (shared chunks,
    // not copies) until the response arrives. Gated like diag_, whose cost
    // also grows with the body.
    ("failover_unary_cpu_us_per_request", 0.20, 10.0),
    ("failover_unary_added_p50_us", 0.20, 40.0),
    ("failover_agent_upload_cpu_ms_per_mb", 0.25, 0.1),
    ("alloc_failover_unary_per_request", 0.02, 0.5),
    ("alloc_failover_unary_bytes_per_request", 0.02, 256.0),
    ("alloc_failover_agent_upload_per_request", 0.15, 2.0),
    ("failover_unary_failed_requests", 0.0, 0.0),
    ("failover_upload_failed_requests", 0.0, 0.0),
    ("failover_agent_upload_failed_requests", 0.0, 0.0),
    // The first account refuses every fourth request before reading it:
    // refused requests move on. An upload refused after its body was
    // delivered whole is ambiguous and answered as it is, which happens
    // rarely (8 in 19,264 in a 10 s load), hence the allowance on uploads;
    // before the hand-back distinguished an upload cut short, 5% failed.
    ("refused_unary_failed_requests", 0.0, 0.0),
    ("refused_upload_failed_requests", 0.0, 3.0),
    ("refused_agent_upload_failed_requests", 0.0, 3.0),
    // OTLP export runs off the data path, so it must cost no request time.
    ("otlp_unary_cpu_us_per_request", 0.20, 10.0),
    ("otlp_unary_added_p50_us", 0.20, 40.0),
    ("otlp_unary_failed_requests", 0.0, 0.0),
    // Stress (TLS path). Cold connections swing 3-9 ms with machine load, so
    // that gate only catches connections set up one after another. Stalled
    // clients: memory per stream is the precise signal (default HTTP/2
    // windows: 3.3 MB, and other streams failed).
    ("tls_cold_unary_added_p50_us", 1.50, 7_000.0),
    ("tls_mixed_event_added_p50_us", 0.50, 1_000.0),
    ("tls_mixed_event_added_p99_us", 0.50, 2_000.0),
    ("tls_mixed_upload_added_p50_us", 0.50, 1_000.0),
    ("tls_stalled_event_added_p99_us", 0.50, 3_000.0),
    ("tls_stalled_rss_kb_per_stream", 0.20, 200.0),
    ("tls_cold_failed_requests", 0.0, 0.0),
    ("tls_stress_failed_requests", 0.0, 0.0),
    // Any failed request or dropped stream is a regression.
    ("unary_failed_requests", 0.0, 0.0),
    ("upload_failed_requests", 0.0, 0.0),
    ("tls_unary_failed_requests", 0.0, 0.0),
    ("tls_upload_failed_requests", 0.0, 0.0),
    ("tls_paced_failed_streams", 0.0, 0.0),
    // Streams that failed or ended short, in the latency and CPU-per-event
    // scenarios of each gateway variant.
    ("sse_failed_streams", 0.0, 0.0),
    ("tls_sse_failed_streams", 0.0, 0.0),
    ("auth_sse_failed_streams", 0.0, 0.0),
    ("models_sse_failed_streams", 0.0, 0.0),
    ("diag_sse_failed_streams", 0.0, 0.0),
    ("otlp_sse_failed_streams", 0.0, 0.0),
    ("openai_sse_failed_streams", 0.0, 0.0),
    ("bedrock_sse_failed_streams", 0.0, 0.0),
    ("failover_sse_failed_streams", 0.0, 0.0),
    ("refused_sse_failed_streams", 0.0, 0.0),
    // Guard the property, not the microseconds: a release that waits for the
    // next upstream write would take seconds.
    ("cancel_upstream_release_max_us", 1.0, 20_000.0),
    ("tls_cancel_upstream_release_max_us", 1.0, 20_000.0),
];

/// Which gated metrics a comparison enforces.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scope {
    /// Name patterns (`alloc_*`, `*_failed_requests`, or an exact name);
    /// empty enforces every gate. The others are still compared, and read
    /// `report` instead of `REGRESSED` past their limit.
    pub only: Vec<String>,
    /// Name patterns of metrics allowed to regress, for an approved tradeoff:
    /// they read `accepted` instead of `REGRESSED` past their limit.
    pub accept: Vec<String>,
    /// A metric the reference lacks passes as `new` instead of failing: the
    /// reference is another commit's run, which may predate the metric.
    pub allow_new: bool,
}

impl Scope {
    fn enforces(&self, name: &str) -> bool {
        (self.only.is_empty() || matches(&self.only, name)) && !matches(&self.accept, name)
    }
}

fn matches(patterns: &[String], name: &str) -> bool {
    patterns
        .iter()
        .any(|p| match (p.strip_prefix('*'), p.strip_suffix('*')) {
            (Some(suffix), _) => name.ends_with(suffix),
            (None, Some(prefix)) => name.starts_with(prefix),
            (None, None) => name == p,
        })
}

/// Prints a comparison table; returns false if any enforced metric
/// regressed or is missing.
pub fn compare(reference: &str, current: &str, scope: &Scope) -> bool {
    let (reference, current) = (load(reference), load(current));
    if reference["machine"] != current["machine"] {
        eprintln!("warning: the reference ran on a different machine; comparison is unreliable");
    }
    println!(
        "{:36} {:>10} {:>10} {:>10}  result",
        "metric", "reference", "current", "limit"
    );
    let mut ok = true;
    for &(name, rel, abs) in GATES {
        let enforced = scope.enforces(name);
        let (b, c) = match (
            reference["metrics"][name].as_f64(),
            current["metrics"][name].as_f64(),
        ) {
            (Some(b), Some(c)) => (b, c),
            (None, Some(c)) if scope.allow_new => {
                println!("{name:36} {:>10} {c:>10.2} {:>10}  new", "-", "-");
                continue;
            }
            // `missing` fails the gate; `absent`, outside the gated set, not.
            _ => {
                println!("{name:36} {}", if enforced { "missing" } else { "absent" });
                ok &= !enforced;
                continue;
            }
        };
        let limit = if stats::counts_failures(name) {
            // An absolute count: failures the reference had are no allowance.
            abs
        } else if name.ends_with("_release_max_us") {
            // A broken reference (5 s, the scenario's timeout) is no allowance
            // either: past a second, the upstream request was not released.
            (b.max(0.0) * (1.0 + rel) + abs).min(RELEASE_CEILING_US)
        } else {
            // Added latency can be near zero or negative (noise); the absolute
            // floor absorbs that.
            b.max(0.0) * (1.0 + rel) + abs
        };
        let pass = c <= limit;
        ok &= pass || !enforced;
        println!(
            "{name:36} {b:>10.2} {c:>10.2} {limit:>10.2}  {}",
            match (pass, enforced) {
                (true, _) => "ok",
                (false, true) => "REGRESSED",
                (false, false) if matches(&scope.accept, name) => "accepted",
                (false, false) => "report",
            }
        );
    }
    ok
}

/// Writes to `out` the report of `runs` (results of one `wye-bench run`
/// each) merged per metric ([`stats::merge`]): rounds alternated with
/// another binary merge as one run of `--repeat` would.
pub fn median(out: &str, runs: &[&str]) {
    let reports: Vec<Value> = runs.iter().map(|path| load(path)).collect();
    let metrics: Vec<BTreeMap<String, f64>> = reports
        .iter()
        .map(|report| {
            report["metrics"]
                .as_object()
                .unwrap()
                .iter()
                // `null`: a gateway produced no samples.
                .map(|(name, value)| (name.clone(), value.as_f64().unwrap_or(f64::NAN)))
                .collect()
        })
        .collect();
    let mut report = reports[0].clone();
    report["repeat"] = runs.len().into();
    report["metrics"] = serde_json::to_value(stats::merge(&metrics)).unwrap();
    std::fs::write(out, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
}

fn load(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}")))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;
    use wye_testkit::TempDir;

    use super::*;

    /// Writes a report of `metrics` as `dir/name`; returns its path.
    fn report(dir: &Path, name: &str, metrics: Value) -> String {
        let path = dir.join(name);
        let report = json!({"machine": {}, "metrics": metrics});
        std::fs::write(&path, report.to_string()).unwrap();
        path.display().to_string()
    }

    #[test]
    fn a_scope_enforces_matching_metrics_only() {
        let all = Scope::default();
        assert!(all.enforces("unary_added_p50_us"));
        let ci = Scope {
            only: vec![
                "alloc_*".into(),
                "*_failed_requests".into(),
                "unary_failed_requests".into(),
            ],
            allow_new: true,
            ..Scope::default()
        };
        assert!(ci.enforces("alloc_unary_per_request"));
        assert!(ci.enforces("tls_upload_failed_requests"));
        assert!(ci.enforces("unary_failed_requests"));
        assert!(!ci.enforces("unary_added_p50_us"));
        assert!(!ci.enforces("tls_paced_failed_streams"));
        let tradeoff = Scope {
            accept: vec!["unary_added_*".into()],
            ..Scope::default()
        };
        assert!(!tradeoff.enforces("unary_added_p50_us"));
        assert!(tradeoff.enforces("alloc_unary_per_request"));
    }

    #[test]
    fn report_only_and_new_metrics_do_not_fail_a_scoped_comparison() {
        let dir = TempDir::new("bench-gate");
        let base = report(
            &dir,
            "base.json",
            json!({"unary_added_p50_us": 100.0, "alloc_unary_per_request": 20.0}),
        );
        let slower = report(
            &dir,
            "slower.json",
            json!({
                "unary_added_p50_us": 900.0,
                "alloc_unary_per_request": 20.0,
                "bedrock_unary_failed_requests": 0.0,
            }),
        );
        let scope = |only: &[&str], allow_new| Scope {
            only: only.iter().map(|s| (*s).to_owned()).collect(),
            allow_new,
            ..Scope::default()
        };
        // Latency is report-only and the new metric passes; gates outside the
        // scope are missing from this fixture but not enforced.
        let fixture = ["alloc_unary_per_request", "bedrock_unary_failed_requests"];
        assert!(compare(&base, &slower, &scope(&fixture, true)));
        // Without --allow-new, the metric the reference lacks fails.
        assert!(!compare(&base, &slower, &scope(&fixture, false)));
        // The full gate fails on the latency regression, unless it is accepted.
        assert!(!compare(&base, &slower, &Scope::default()));
        let latency = ["unary_added_p50_us", "alloc_unary_per_request"];
        assert!(!compare(&base, &slower, &scope(&latency, false)));
        let accepted = Scope {
            accept: vec!["unary_added_p50_us".into()],
            ..scope(&latency, false)
        };
        assert!(compare(&base, &slower, &accepted));
        let heavier = report(
            &dir,
            "heavier.json",
            json!({"unary_added_p50_us": 100.0, "alloc_unary_per_request": 22.0}),
        );
        assert!(!compare(
            &base,
            &heavier,
            &scope(&["alloc_unary_per_request"], true)
        ));
    }

    #[test]
    fn failures_and_release_times_have_absolute_limits() {
        let dir = TempDir::new("bench-abs");
        let only = |name: &str| Scope {
            only: vec![name.into()],
            ..Scope::default()
        };
        // The reference failing 5 requests allows the current commit none.
        let failing = report(&dir, "failing.json", json!({"unary_failed_requests": 5.0}));
        let fewer = report(&dir, "fewer.json", json!({"unary_failed_requests": 3.0}));
        assert!(!compare(&failing, &fewer, &only("unary_failed_requests")));
        let none = report(&dir, "none.json", json!({"unary_failed_requests": 0.0}));
        assert!(compare(&failing, &none, &only("unary_failed_requests")));
        // A reference stuck at the 5 s timeout allows no more than a second.
        let metric = "cancel_upstream_release_max_us";
        let stuck = report(&dir, "stuck.json", json!({metric: 5_000_000.0}));
        let slow = report(&dir, "slow.json", json!({metric: 2_000_000.0}));
        assert!(!compare(&stuck, &slow, &only(metric)));
    }

    #[test]
    fn median_merges_rounds_into_one_report() {
        let dir = TempDir::new("bench-median");
        let rounds: Vec<String> = [(3.0, Some(0.0)), (1.0, None), (2.0, Some(2.0))]
            .iter()
            .enumerate()
            .map(|(i, (latency, failed))| {
                let path = dir.join(format!("round-{i}.json")).display().to_string();
                let report = json!({
                    "machine": {"os": "test"},
                    "repeat": 1,
                    "metrics": {"unary_added_p50_us": latency, "unary_failed_requests": failed},
                });
                std::fs::write(&path, report.to_string()).unwrap();
                path
            })
            .collect();
        let out = dir.join("median.json").display().to_string();
        median(&out, &rounds.iter().map(String::as_str).collect::<Vec<_>>());
        let merged = load(&out);
        assert_eq!(merged["machine"]["os"], "test");
        assert_eq!(merged["repeat"], 3);
        assert_eq!(merged["metrics"]["unary_added_p50_us"], 2.0);
        // Failure counts take the worst round, and a round without samples
        // is worse than any: one round of three failing still fails.
        assert!(merged["metrics"]["unary_failed_requests"].is_null());
    }
}
