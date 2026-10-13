//! `wye-bench run`: measures every scenario on both upstream paths and writes the report.

use std::{collections::BTreeMap, path::Path, time::Duration};

use serde_json::{Value, json};
use wye_testkit::TestCa;

use crate::{
    client::Target,
    gateway_process::{Gateway, GatewayProcess},
    mock,
    scenarios::{self, Stream},
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

/// One way of reaching the mock upstream.
struct UpstreamPath<'a> {
    /// Metric name prefix; empty for the plain path to keep historical names.
    prefix: &'static str,
    name: &'static str,
    direct: Target,
    base_url: String,
    ca_pem: Option<&'a str>,
}

/// Runs every scenario `repeat` times and writes the per-metric median to `out`.
/// With `krakend`, the same scenarios also run through KrakenD (`krakend_` metrics).
pub async fn run(wye: &str, krakend: Option<&str>, out: &str, repeat: usize) {
    let mut gateways = vec![Gateway::Wye(wye)];
    gateways.extend(krakend.map(Gateway::KrakenD));
    let dir = Path::new(out)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).unwrap();

    let ca = TestCa::generate();
    let plain = mock::spawn(None).await;
    let tls = mock::spawn(Some(&ca)).await;
    let paths = [
        UpstreamPath {
            prefix: "",
            name: "plain",
            direct: Target::plain(plain),
            base_url: format!("http://{plain}"),
            ca_pem: None,
        },
        UpstreamPath {
            prefix: "tls_",
            name: "tls",
            direct: Target::tls(tls, ca.ca_cert()),
            base_url: format!("https://{tls}"),
            ca_pem: Some(ca.ca_pem()),
        },
    ];

    let mut runs = Vec::with_capacity(repeat);
    for i in 1..=repeat {
        let mut metrics = BTreeMap::new();
        for path in &paths {
            let direct = Direct::measure(&path.direct).await;
            put(
                &mut metrics,
                path.prefix,
                "direct_throughput_mb_s",
                direct.mb_s,
            );
            for (label, p) in PERCENTILES {
                let name = format!("direct_unary_{label}_us");
                put(
                    &mut metrics,
                    path.prefix,
                    &name,
                    percentile(&direct.latency.unary, p),
                );
            }
            for &gateway in &gateways {
                eprintln!(
                    "run {i}/{repeat}: {} upstream, {}",
                    path.name,
                    gateway.name()
                );
                // A fresh process per run, so memory retained from earlier runs cannot hide new allocations.
                let process =
                    GatewayProcess::start(gateway, &path.base_url, path.ca_pem, dir, path.name)
                        .await;
                let prefix = format!("{}{}", path.prefix, gateway.prefix());
                measure(
                    &direct,
                    &process,
                    &prefix,
                    gateway.streams_sse(),
                    &mut metrics,
                )
                .await;
            }
        }
        runs.push(metrics);
    }

    let report = json!({
        "machine": machine(),
        "repeat": repeat,
        "paths": {"": "plain HTTP/1.1 upstream", "tls_": "TLS + HTTP/2 upstream"},
        "scenarios": {
            "unary": {"concurrency": UNARY_CONCURRENCY, "requests": UNARY_TOTAL, "response_bytes": 512},
            "sse": {"concurrency": SSE_CONCURRENCY, "streams": SSE_STREAMS, "events": SSE_EVENTS, "interval_us": SSE_INTERVAL_US},
            "throughput": {"concurrency": THROUGHPUT_CONCURRENCY, "streams": THROUGHPUT_STREAMS, "events": THROUGHPUT_EVENTS, "event_bytes": THROUGHPUT_EVENT_BYTES},
            "idle": {"streams": IDLE_STREAMS},
            "cancel": {"rounds": CANCEL_ROUNDS},
        },
        "metrics": median(&runs),
    });
    std::fs::write(out, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
    for (name, value) in report["metrics"].as_object().unwrap() {
        println!("{name:40} {value}");
    }
    if krakend.is_some() {
        print_versus(&report["metrics"]);
    }
}

const PERCENTILES: [(&str, f64); 2] = [("p50", 50.0), ("p99", 99.0)];

fn put(metrics: &mut BTreeMap<String, f64>, prefix: &str, name: &str, value: f64) {
    metrics.insert(format!("{prefix}{name}"), (value * 100.0).round() / 100.0);
}

/// Wye against KrakenD for each metric both measured; lower is better except MB/s.
fn print_versus(metrics: &Value) {
    println!("\n{:36} {:>10} {:>10}  winner", "metric", "wye", "krakend");
    let metrics = metrics.as_object().unwrap();
    for path in ["", "tls_"] {
        for (name, wye) in metrics {
            let Some(metric) = name.strip_prefix(path) else {
                continue;
            };
            if metric.starts_with("tls_")
                || metric.starts_with("krakend_")
                || metric.starts_with("direct_")
            {
                continue;
            }
            let Some(krakend) = metrics.get(&format!("{path}krakend_{metric}")) else {
                continue;
            };
            let (wye, krakend) = (wye.as_f64().unwrap(), krakend.as_f64().unwrap());
            let higher_is_better = metric.ends_with("_mb_s");
            let wye_wins = if higher_is_better {
                wye >= krakend
            } else {
                wye <= krakend
            };
            println!(
                "{name:36} {wye:>10.2} {krakend:>10.2}  {}",
                if wye_wins { "wye" } else { "KRAKEND" }
            );
        }
    }
}

/// The direct baseline of one path: what a client sees without a gateway.
struct Direct {
    latency: Latency,
    mb_s: f64,
}

impl Direct {
    async fn measure(target: &Target) -> Self {
        let latency = Latency::measure(target).await;
        let (mb_s, _) = throughput(target).await;
        Self { latency, mb_s }
    }
}

/// Measures `process` against the `direct` baseline, adding `prefix`ed metrics.
async fn measure(
    direct: &Direct,
    process: &GatewayProcess,
    prefix: &str,
    streams_sse: bool,
    metrics: &mut BTreeMap<String, f64>,
) {
    let via = Latency::measure(process.target()).await;
    let cpu_before = process.cpu_secs();
    let (mb_s, mb) = throughput(process.target()).await;
    let cpu_after = process.cpu_secs();

    let direct = &direct.latency;
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
            added(&via.unary, &direct.unary),
        );
        put(
            metrics,
            prefix,
            &format!("sse_ttfb_added_{label}_us"),
            added(&via.ttfb, &direct.ttfb),
        );
        put(
            metrics,
            prefix,
            &format!("sse_event_added_{label}_us"),
            added(&via.event_delay, &direct.event_delay),
        );
    }
    put(metrics, prefix, "via_throughput_mb_s", mb_s);
    // `ps` reports CPU time in 10 ms steps; the scenario streams enough data to make that negligible.
    put(
        metrics,
        prefix,
        "throughput_cpu_ms_per_mb",
        (cpu_after - cpu_before) * 1_000.0 / mb,
    );

    put(metrics, prefix, "rss_after_load_kb", process.rss_kb());
    if !streams_sse {
        return;
    }
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
struct Latency {
    unary: Vec<u64>,
    ttfb: Vec<u64>,
    event_delay: Vec<u64>,
}

impl Latency {
    async fn measure(target: &Target) -> Self {
        // Warm up connection pools and allocators.
        scenarios::unary(target, UNARY_CONCURRENCY, 2_000).await;
        scenarios::sse(target, SSE_CONCURRENCY, 128, 5, 64, 0).await;

        let unary = sorted(scenarios::unary(target, UNARY_CONCURRENCY, UNARY_TOTAL).await);
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

fn median(runs: &[BTreeMap<String, f64>]) -> BTreeMap<String, f64> {
    runs[0]
        .keys()
        .map(|name| {
            let mut values: Vec<f64> = runs.iter().map(|r| r[name]).collect();
            values.sort_by(f64::total_cmp);
            (name.clone(), values[values.len() / 2])
        })
        .collect()
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    sorted[((p / 100.0) * (sorted.len() - 1) as f64).round() as usize] as f64
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

fn machine() -> Value {
    let sysctl = |key: &str| {
        std::process::Command::new("sysctl")
            .args(["-n", key])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    };
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu": sysctl("machdep.cpu.brand_string"),
        "cpus": std::thread::available_parallelism().map_or(0, |n| n.get()),
    })
}
