//! Wye benchmark and regression gate.
//!
//! Runs the same scenarios directly against a mock Anthropic upstream and
//! through a real `wye` binary, then reports Wye's added cost. Two upstream
//! paths are measured: plain HTTP/1.1 (metric names without prefix) and TLS
//! with HTTP/2, like the real provider (`tls_` prefix), which also runs the
//! token-paced and stress scenarios (cold connections, mixed load, stalled
//! clients). With `--krakend`, the
//! same scenarios also run through KrakenD (`krakend_` prefix); with `--litellm`,
//! through LiteLLM's pass-through endpoint (`litellm_` prefix); with `--bifrost`,
//! through Bifrost's Anthropic route as Claude Code (`bifrost_` prefix).
//! Wye also runs on the plain path with `[auth]` (`auth_`), model permissions
//! (`models_`), cache diagnostics (`diag_`), and OTLP export (`otlp_`).
//! With `--wye-alloc`, a `--features alloc-stats` build of Wye adds heap allocation
//! metrics (`alloc_` prefix).
//! `WYE_BENCH_UPSTREAM` appends lines to Wye's `[upstream]` config, for tuning experiments.
//! `WYE_BENCH_GATEWAY_LOG=1` writes each gateway's output to a log next to its config.
//! `WYE_BENCH_BIFROST_POOLS=default` runs Bifrost with its default pool sizes.
//!
//! Usage:
//!
//! ```text
//! wye-bench run --wye <path> --out <results.json> [--repeat N] [--wye-alloc <path>] [--krakend <path>] [--litellm <path>] [--bifrost <path>]
//! wye-bench compare <baseline.json> <results.json>
//! wye-bench adopt-new <baseline.json> <results.json>
//! wye-bench load (--wye | --wye-openai | --krakend) <path> --scenario <upload|tls-upload|paced|capacity|events> --seconds N
//! ```

// Development tool, not on the request path: panics abort a benchmark run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod allocations;
mod capacity;
mod client;
mod gate;
mod gateway_config;
mod gateway_process;
mod load;
mod measure;
mod mock;
mod run;
mod scenarios;
mod stats;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["run", "--wye", binary, "--out", out, flags @ ..] => {
            let mut repeat = 3;
            let mut others = Vec::new();
            let mut wye_alloc = None;
            let mut rest: &[&str] = flags;
            while let [flag, value, tail @ ..] = rest {
                match *flag {
                    "--repeat" => match value.parse() {
                        Ok(n) if n > 0 => repeat = n,
                        _ => return usage(),
                    },
                    "--krakend" => others.push(gateway_process::Gateway::KrakenD(value)),
                    "--litellm" => others.push(gateway_process::Gateway::LiteLlm(value)),
                    "--bifrost" => others.push(gateway_process::Gateway::Bifrost(value)),
                    "--wye-alloc" => wye_alloc = Some(*value),
                    _ => return usage(),
                }
                rest = tail;
            }
            if !rest.is_empty() {
                return usage();
            }
            mock::init_clock();
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run::run(binary, wye_alloc, &others, out, repeat));
            ExitCode::SUCCESS
        }
        ["compare", baseline, current] => {
            if gate::compare(baseline, current) {
                ExitCode::SUCCESS
            } else {
                eprintln!("benchmark regression: do not commit this change");
                ExitCode::FAILURE
            }
        }
        [
            "load",
            kind,
            binary,
            "--scenario",
            scenario,
            "--seconds",
            seconds,
        ] => {
            let gateway = match *kind {
                "--wye" => gateway_process::Gateway::Wye(binary),
                "--wye-openai" => gateway_process::Gateway::WyeOpenai(binary),
                "--krakend" => gateway_process::Gateway::KrakenD(binary),
                _ => return usage(),
            };
            let Ok(seconds) = seconds.parse() else {
                return usage();
            };
            mock::init_clock();
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(load::load(gateway, scenario, seconds));
            ExitCode::SUCCESS
        }
        ["adopt-new", baseline, current] => {
            gate::adopt_new(baseline, current);
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  wye-bench run --wye <path> --out <results.json> [--repeat N] [--wye-alloc <path>] [--krakend <path>] [--litellm <path>] [--bifrost <path>]\n  \
         wye-bench compare <baseline.json> <results.json>\n  \
         wye-bench adopt-new <baseline.json> <results.json>\n  \
         wye-bench load (--wye | --wye-openai | --krakend) <path> --scenario <upload|tls-upload|paced|capacity|events> --seconds N"
    );
    ExitCode::from(2)
}
