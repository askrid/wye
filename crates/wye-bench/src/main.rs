//! Wye benchmark and regression gate.
//!
//! Runs the same scenarios directly against a mock Anthropic upstream and
//! through a real `wye` binary, and reports what Wye adds: on a plain HTTP/1.1
//! path (no metric prefix) and on TLS with HTTP/2 (`tls_`), plus the Wye
//! variants, `--wye-alloc` allocation metrics (`alloc_`), and competitors
//! (`--krakend`, `--litellm`, `--bifrost`). `docs/development.md` lists each
//! scenario.
//!
//! Knobs for experiments: `WYE_BENCH_UPSTREAM` appends lines to Wye's
//! `[upstream]` config, `WYE_BENCH_GATEWAY_LOG=1` keeps each gateway's output
//! in a log next to its config, `WYE_BENCH_BIFROST_POOLS=default` runs Bifrost
//! with its default pool sizes. Without arguments it prints its usage.

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

use gateway_process::{Gateway, Variant};

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
                    "--krakend" => others.push(Gateway::KrakenD(value)),
                    "--litellm" => others.push(Gateway::LiteLlm(value)),
                    "--bifrost" => others.push(Gateway::Bifrost(value)),
                    "--wye-alloc" => wye_alloc = Some(*value),
                    _ => return usage(),
                }
                rest = tail;
            }
            if !rest.is_empty() {
                return usage();
            }
            block_on(run::run(binary, wye_alloc, &others, out, repeat))
        }
        ["compare", reference, current, flags @ ..] => {
            let Some(scope) = compare_scope(flags) else {
                return usage();
            };
            if gate::compare(reference, current, &scope) {
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
                "--wye" => Gateway::Wye(binary, Variant::Plain),
                "--wye-openai" => Gateway::Wye(binary, Variant::Openai),
                "--wye-refused" => Gateway::Wye(binary, Variant::Refused),
                "--krakend" => Gateway::KrakenD(binary),
                _ => return usage(),
            };
            let Ok(seconds) = seconds.parse() else {
                return usage();
            };
            block_on(load::load(gateway, scenario, seconds))
        }
        ["median", out, runs @ ..] if !runs.is_empty() => {
            gate::median(out, runs);
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}

/// Runs `work` to its end, on the clock the mock and the client share.
fn block_on(work: impl Future<Output = ()>) -> ExitCode {
    mock::init_clock();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
    ExitCode::SUCCESS
}

/// `[--only <pattern,...>] [--accept <pattern,...>] [--allow-new]` for `compare`.
fn compare_scope(flags: &[&str]) -> Option<gate::Scope> {
    let mut scope = gate::Scope::default();
    let mut rest = flags;
    loop {
        match rest {
            [] => return Some(scope),
            ["--only", patterns, tail @ ..] => {
                scope.only = patterns.split(',').map(str::to_owned).collect();
                rest = tail;
            }
            ["--accept", patterns, tail @ ..] => {
                scope.accept = patterns.split(',').map(str::to_owned).collect();
                rest = tail;
            }
            ["--allow-new", tail @ ..] => {
                scope.allow_new = true;
                rest = tail;
            }
            _ => return None,
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  wye-bench run --wye <path> --out <results.json> [--repeat N] [--wye-alloc <path>] [--krakend <path>] [--litellm <path>] [--bifrost <path>]\n  \
         wye-bench compare <reference.json> <results.json> [--only <pattern,...>] [--accept <pattern,...>] [--allow-new]\n  \
         wye-bench median <out.json> <results.json>...\n  \
         wye-bench load (--wye | --wye-openai | --wye-refused | --krakend) <path> --scenario <upload|tls-upload|paced|capacity|events> --seconds N"
    );
    ExitCode::from(2)
}
