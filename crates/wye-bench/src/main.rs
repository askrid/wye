//! Wye benchmark and regression gate.
//!
//! Runs the same scenarios directly against a mock Anthropic upstream and
//! through a real `wye` binary, then reports Wye's added cost. Two upstream
//! paths are measured: plain HTTP/1.1 (metric names without prefix) and TLS
//! with HTTP/2, like the real provider (`tls_` prefix).
//!
//! Usage:
//!   wye-bench run --wye <path> --out <results.json> [--repeat N]
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
mod mock;
mod run;
mod scenarios;
mod wye_process;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["run", "--wye", binary, "--out", out, rest @ ..] => {
            let repeat = match rest {
                [] => 3,
                ["--repeat", n] => match n.parse() {
                    Ok(n) if n > 0 => n,
                    _ => return usage(),
                },
                _ => return usage(),
            };
            mock::init_clock();
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run::run(binary, out, repeat));
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
        "usage:\n  wye-bench run --wye <path> --out <results.json> [--repeat N]\n  \
         wye-bench compare <baseline.json> <results.json>\n  \
         wye-bench adopt-new <baseline.json> <results.json>"
    );
    ExitCode::from(2)
}
