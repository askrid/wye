# Development

How changes are made, verified, and committed. Speed and statelessness are the product: Wye must never be the bottleneck between a client and the provider, so performance is checked on every commit, like correctness.

## Layout

| Path | Contents |
|---|---|
| `crates/gateway-core` | Portable policy: config validation, credential verification, model permissions and token ceilings, request-body and SSE scanners, prompt hashes, agent headers. No I/O, no Tokio. |
| `crates/wye` | The binary: CLI (`main.rs`, `token.rs`, `explain.rs`, `policy_test.rs`, `report.rs`, `wrapper.rs`, `probe.rs`), startup resolution, listener and shutdown (`server.rs`), routing (`gateway.rs`), credentials and permissions, forwarding (`upstream/`), usage log and OTLP export (`usage_log/`). Module map in [architecture.md](architecture.md#repository-and-workspace) |
| `crates/wye-testkit` | Test support: throwaway CA, test signing keys, mock upstream (plain, or TLS with HTTP/2) |
| `crates/wye-bench` | Benchmark and regression gate; mock Anthropic upstream, load client, soak and capacity runs, competitor runs |
| `bench/baseline.json` | Gated benchmark baseline for the development machine |
| `scripts/check.sh`, `scripts/ab.sh` | Commit gate; A/B comparison of two binaries |
| `scripts/upload-latency.sh` | Upload latency over HTTP/1.1 and HTTP/2 against the real API (needs an API key; not part of the gate) |
| `Dockerfile`, `scripts/image-smoke.sh` | OCI image; its build-and-run smoke test (needs Docker; not part of the commit gate) |

## Commit gate

`scripts/check.sh` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, all tests, a release build plus an allocation-counting build (`--features alloc-stats`, in `target/alloc`), and the benchmark, then compares the benchmark with `bench/baseline.json`. The pre-commit hook runs it for every commit that touches more than docs; enable it once per clone:

```bash
git config core.hooksPath .githooks
```

- **A change that fails the check is not committed**, including any benchmark regression. Fix it or drop it; never weaken a test, a tolerance, or the baseline to pass.
- `scripts/check.sh --update-baseline` records a new baseline only for an intended, reviewed tradeoff, with the before/after numbers and the reason in the commit message. Never commit with `--no-verify`.
- New metrics enter the baseline with `wye-bench adopt-new`, which adds them without touching existing values.
- The baseline is machine-specific: on a new machine, record a fresh one from an unchanged checkout first.

A check takes about 100 seconds after an ordinary code change: tests 11 s, incremental builds a few seconds, benchmark about 80 s (one run takes about 27 s; `wye-bench` prints the duration of each phase to stderr). Changes to dependencies take several minutes longer, because both release targets rebuild from scratch. Each benchmark is the median of 3 runs, and a failed comparison is re-run once before it counts, because laptop noise can push a single run past a limit. The gate reliably blocks about 100 µs of added latency per request and about 25% more CPU per unit of work; smaller effects need `scripts/ab.sh <wye-a> <wye-b> [rounds]`, which runs two binaries in alternating rounds. Adopt an optimization only when it wins beyond noise in two A/B runs and worsens no gated metric. Results and rejected experiments are recorded in [research.md](research.md).

## Benchmark

`wye-bench run --wye <path> --out <results.json>` measures each scenario directly against the mock and through the gateway, on a plain HTTP/1.1 upstream and on TLS + HTTP/2 (`tls_` metrics), plus, on the plain path, Wye with `[auth]` (`auth_` metrics, per-request scenarios only), Wye with model permissions (`models_` metrics, per-request and upload scenarios), Wye with `[cache_diagnostics]` (`diag_` metrics, per-request and upload scenarios), and Wye exporting OTLP to the mock (`otlp_` metrics, per-request scenarios; reported, not gated). It reports what the gateway adds: latency (unary, time to first byte, per SSE event), CPU per request, per MB, and per event, memory per idle and active stream, upstream release after a disconnect, uploads (one long string, and on the plain path `agent_upload`: the same size shaped like a Claude Code request, which costs the diagnostics scanner more), 2,000 token-paced streams, and failed requests (gated at zero). `--krakend <path>` and `--litellm <path>` run the same scenarios through those gateways and print a comparison table.

`--wye-alloc <path>` takes a Wye build with `--features alloc-stats`, whose global allocator counts every heap allocation and serves the counters at `GET /debug/alloc` (that build only; never ship it). It adds `alloc_` metrics: allocations and bytes per request, allocations per streamed event and per stream setup, live heap per idle stream (also after a 512 KB request, and on the TLS upstream), and allocations per 1 MiB upload (also for the agent-shaped body, with and without cache diagnostics). Counts are nearly deterministic (under 1% spread, uploads about 5%), so the gate catches a single extra allocation per request, stream, or event that CPU timing would never resolve.

For diagnosis beyond these numbers, `wye-bench load (--wye | --krakend) <path> --scenario <upload|tls-upload|paced|capacity> --seconds N` keeps one scenario running against one gateway and prints its pid and cost, so it can be profiled with macOS `sample <pid>` or Linux `perf`. With `paced` it is a soak test: it prints resident memory, and with an `alloc-stats` build the live heap, after every batch of 2,000 streams. With `capacity` it runs Wye on one Tokio worker thread (`WYE_BENCH_GATEWAY_THREADS=N` for more) and steps the number of paced streams up, printing events per second, Wye's CPU, and the p99 event delay Wye adds over a direct run, and the largest step within 10 ms added p99 (`--seconds` is ignored).

## Code changes

- Correctness and protocol compatibility first; then no unnecessary allocation, buffering, locks, or blocking on the request path.
- Every change ends with a review pass over the files it touched: names, duplication, dead code, stale comments, module boundaries. Refactor instead of patching when logic repeats, a module does two jobs, or a name no longer fits.
- A refactor changes no behavior, keeps existing tests unmodified, and is committed separately with "No behavior change" in the message.
- Tests are deterministic: local mock upstream, no paid provider calls. Parsers of untrusted input get property tests.
- `unwrap`, `expect`, panics, and indexing are denied outside tests; `unsafe` is forbidden in `gateway-core` and denied elsewhere.
