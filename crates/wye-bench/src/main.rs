//! Wye benchmark and regression gate.
//!
//! Runs the same scenarios directly against a mock Anthropic upstream and
//! through a real `wye` binary, then reports Wye's added cost. Two upstream
//! paths are measured: plain HTTP/1.1 (metric names without prefix) and TLS
//! with HTTP/2, like the real provider (`tls_` prefix). With `--krakend`, the
//! same scenarios also run through KrakenD (`krakend_` prefix); with `--litellm`,
//! through LiteLLM's pass-through endpoint (`litellm_` prefix).
//! `WYE_BENCH_UPSTREAM` appends lines to Wye's `[upstream]` config, for tuning experiments.
//! `WYE_BENCH_GATEWAY_LOG=1` writes each gateway's output to a log next to its config.
//!
//! Usage:
//!   wye-bench run --wye <path> --out <results.json> [--repeat N] [--krakend <path>] [--litellm <path>]
//!   wye-bench compare <baseline.json> <results.json>
//!   wye-bench adopt-new <baseline.json> <results.json>

// Development tool, not on the request path: panics abort a benchmark run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod client;
mod gate;
mod gateway_process;
mod mock;
mod run;
mod scenarios;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["run", "--wye", binary, "--out", out, flags @ ..] => {
            let mut repeat = 3;
            let mut others = Vec::new();
            let mut rest: &[&str] = flags;
            while let [flag, value, tail @ ..] = rest {
                match *flag {
                    "--repeat" => match value.parse() {
                        Ok(n) if n > 0 => repeat = n,
                        _ => return usage(),
                    },
                    "--krakend" => others.push(gateway_process::Gateway::KrakenD(value)),
                    "--litellm" => others.push(gateway_process::Gateway::LiteLlm(value)),
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
                .block_on(run::run(binary, &others, out, repeat));
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
        ["adopt-new", baseline, current] => {
            gate::adopt_new(baseline, current);
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  wye-bench run --wye <path> --out <results.json> [--repeat N] [--krakend <path>] [--litellm <path>]\n  \
         wye-bench compare <baseline.json> <results.json>\n  \
         wye-bench adopt-new <baseline.json> <results.json>"
    );
    ExitCode::from(2)
}
