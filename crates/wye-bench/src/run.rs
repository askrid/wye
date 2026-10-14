//! `wye-bench run`: measures every scenario on both upstream paths and writes the report.

use std::{collections::BTreeMap, path::Path};

use serde_json::{Value, json};
use wye_testkit::TestCa;

use crate::{
    allocations,
    client::Target,
    gateway_process::{Gateway, GatewayProcess},
    measure::{self, Direct, Scope},
    mock,
    stats::{PERCENTILES, median, percentile, phase, put},
};

/// One way of reaching the mock upstream.
struct UpstreamPath<'a> {
    /// Metric name prefix; empty for the plain path to keep historical names.
    prefix: &'static str,
    name: &'static str,
    direct: Target,
    base_url: String,
    ca_pem: Option<&'a str>,
    /// Runs the token-paced scenario. Only on TLS + HTTP/2, the production
    /// path: over HTTP/1.1 every stream opens its own connections, and
    /// repeated runs exhaust the laptop's ephemeral ports.
    paced: bool,
    /// Also measures the Wye variants with `[auth]` (per-request scenarios)
    /// and `[cache_diagnostics]` (per-request and upload scenarios).
    variants: bool,
}

/// Runs every scenario `repeat` times and writes the per-metric median to `out`.
/// Competitors in `others` run the same scenarios under their own metric prefix.
/// With `wye_alloc` (a `--features alloc-stats` build), allocation metrics are added.
pub async fn run(
    wye: &str,
    wye_alloc: Option<&str>,
    others: &[Gateway<'_>],
    out: &str,
    repeat: usize,
) {
    let mut gateways = vec![Gateway::Wye(wye)];
    gateways.extend_from_slice(others);
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
            paced: false,
            variants: true,
        },
        UpstreamPath {
            prefix: "tls_",
            name: "tls",
            direct: Target::tls(tls, ca.ca_cert()),
            base_url: format!("https://{tls}"),
            ca_pem: Some(ca.ca_pem()),
            paced: true,
            variants: false,
        },
    ];

    let mut runs = Vec::with_capacity(repeat);
    for i in 1..=repeat {
        let mut metrics = BTreeMap::new();
        for path in &paths {
            let direct = phase(
                format!("{}direct", path.prefix),
                Direct::measure(&path.direct, path.paced),
            )
            .await;
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
            let variants = [
                Gateway::WyeAuth(wye),
                Gateway::WyeModels(wye),
                Gateway::WyeDiag(wye),
            ];
            let variants = if path.variants { &variants[..] } else { &[] };
            for &gateway in gateways.iter().chain(variants) {
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
                let scope = Scope {
                    streams_sse: gateway.streams_sse(),
                    paced: path.paced,
                    per_request_only: matches!(
                        gateway,
                        Gateway::WyeAuth(_) | Gateway::WyeModels(_) | Gateway::WyeDiag(_)
                    ),
                    uploads: matches!(gateway, Gateway::WyeModels(_) | Gateway::WyeDiag(_)),
                };
                measure::measure(&direct, &process, &prefix, scope, &mut metrics).await;
            }
        }
        if let Some(binary) = wye_alloc {
            eprintln!("run {i}/{repeat}: allocations");
            phase(
                "allocations",
                allocations::measure(
                    binary,
                    &paths[0].base_url,
                    &paths[1].base_url,
                    ca.ca_pem(),
                    dir,
                    &mut metrics,
                ),
            )
            .await;
        }
        runs.push(metrics);
    }

    let report = json!({
        "machine": machine(),
        "repeat": repeat,
        "paths": {"": "plain HTTP/1.1 upstream", "tls_": "TLS + HTTP/2 upstream"},
        "scenarios": measure::scenario_report(),
        "metrics": median(&runs),
    });
    std::fs::write(out, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
    for (name, value) in report["metrics"].as_object().unwrap() {
        println!("{name:40} {value}");
    }
    for other in others {
        print_versus(&report["metrics"], *other);
    }
}

/// Wye against `other` for each metric both measured; lower is better except MB/s.
fn print_versus(metrics: &Value, other: Gateway<'_>) {
    let name = other.name();
    println!("\n{:36} {:>10} {:>10}  winner", "metric", "wye", name);
    let metrics = metrics.as_object().unwrap();
    for path in ["", "tls_"] {
        for (metric_name, wye) in metrics {
            let Some(metric) = metric_name.strip_prefix(path) else {
                continue;
            };
            if metric.starts_with("tls_")
                || metric.starts_with("krakend_")
                || metric.starts_with("direct_")
            {
                continue;
            }
            let Some(other_value) = metrics.get(&format!("{path}{}{metric}", other.prefix()))
            else {
                continue;
            };
            // Null when a gateway produced no samples (every request failed).
            let (Some(wye), Some(other)) = (wye.as_f64(), other_value.as_f64()) else {
                println!("{metric_name:36} {:>10} {:>10}  n/a", wye, other_value);
                continue;
            };
            let higher_is_better = metric.ends_with("_mb_s");
            let wye_wins = if higher_is_better {
                wye >= other
            } else {
                wye <= other
            };
            println!(
                "{metric_name:36} {wye:>10.2} {other:>10.2}  {}",
                if wye_wins { "wye" } else { loser_label(name) }
            );
        }
    }
}

/// Upper-cased name of the competitor, so losses stand out in the table.
fn loser_label(name: &str) -> &'static str {
    match name {
        "krakend" => "KRAKEND",
        "litellm" => "LITELLM",
        _ => "OTHER",
    }
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
