# Development

How changes are made, verified, and committed. Speed and statelessness are the product: Wye must never be the bottleneck between a client and the provider, so performance is checked on every commit, like correctness.

## Layout

| Path | Contents |
|---|---|
| `crates/gateway-core` | Portable policy: config validation, credential verification, model aliases, permissions and output-token ceilings, pricing, upgrade-check rules, request-body and SSE scanners, prompt hashes, agent headers. No I/O, no Tokio. |
| `crates/wye` | The binary: `main.rs`, the offline commands (`cli/`), startup resolution, listener and shutdown (`server.rs`), routing (`gateway.rs`), credentials and permissions, forwarding (`upstream/`), usage log and OTLP export (`usage_log/`). Module map in [architecture.md](architecture.md#repository-and-workspace) |
| `crates/wye-testkit` | Test support: throwaway CA, test signing keys, mock upstream (plain, or TLS with HTTP/2) |
| `crates/wye-bench` | Benchmark and regression gate; mock Anthropic upstream, load client, soak and capacity runs, competitor runs |
| `bench/baseline.json` | Recorded benchmark results from the development laptop, the numbers the docs cite; the gate does not read it |
| `examples/` | The configuration reference (`wye.toml`), a subscription config, and policy-test cases |
| `scripts/check.sh`, `scripts/bench-against.sh`, `scripts/ab.sh` | Commit gate; this checkout against a commit in alternating rounds; A/B comparison of two binaries |
| `scripts/ci-bench.sh` | The pull-request benchmark: a commit against its base on one machine |
| `scripts/upload-latency.sh` | Upload latency over HTTP/1.1 and HTTP/2 against the real API (needs an API key; not part of the gate) |
| `Dockerfile`, `scripts/image-smoke.sh` | OCI image; its build-and-run smoke test (needs Docker; not part of the commit gate) |
| `.github/workflows/`, `dist-workspace.toml`, `deny.toml` | CI and release workflows, the release build's settings, the dependency policy |

## Commit gate

`scripts/check.sh` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, all tests, a release build plus an allocation-counting build (`--features alloc-stats`, in `target/alloc`), and the benchmark of this checkout against its parent commit. The pre-commit hook runs it for every commit that touches more than docs; enable it once per clone:

```bash
git config core.hooksPath .githooks
```

**Against the parent, not a stored result.** `scripts/bench-against.sh` builds the parent in a worktree (`target/reference`, kept between checks, so it rebuilds only what the last commit changed), runs both commits in alternating rounds, three each, each commit with its own `wye-bench`, and the gate compares the medians. A stored result carries the machine's state on the day it was recorded: the binary that recorded 205 µs of added time to first byte measured 406 µs on the same laptop four days later ([research.md](research.md#gate-method-the-parent-in-alternating-rounds)). Rounds minutes apart share the machine's state, so the gate needs nothing recorded first and runs on any machine.

- **A change that fails the check is not committed**, including any benchmark regression. Fix it or drop it; never weaken a test or a tolerance to pass. Never commit with `--no-verify`.
- `WYE_GATE_ACCEPT=<pattern,...>` lets the named metrics regress (they read `accepted`), only for an intended, reviewed tradeoff, with the before/after numbers and the reason in the commit message: `WYE_GATE_ACCEPT=tls_upload_cpu_ms_per_mb git commit`.
- A metric the parent does not measure reads `new` and is gated from the next commit on.
- Each commit is compared with its parent only, so regressions that each stay under a tolerance can add up. `WYE_GATE_AGAINST=<ref> scripts/check.sh` compares with any commit: with the last release before tagging the next. An amend is compared with the commit it replaces, unless run with `WYE_GATE_AGAINST=HEAD~1`.
- `bench/baseline.json` is refreshed by hand on an idle machine, together with the numbers in the docs: `target/release/wye-bench run --wye target/release/wye --out bench/baseline.json --wye-alloc target/alloc/release/wye --repeat 7`.
- The proxy tests and the benchmark open thousands of sockets. `check.sh` and CI raise the open-file limit to 8192; on macOS, whose default is 256, run `ulimit -n 8192` before a bare `cargo test`. The stalled-client test keeps 256 loopback connections full, and their client ends autotune large receive buffers: more than the 96 MB network pool of a 7 GB macOS CI runner, so CI caps receive-buffer autotuning there (`net.inet.tcp.autorcvbufmax`).

A check takes about 8 minutes after an ordinary code change: tests about 10 s, incremental builds of both commits about a minute, benchmark about 420 s (six runs of about 70 s: Wye and eight variants on the plain path, then the TLS path; `wye-bench` prints the duration of each phase to stderr). The first check in a checkout builds the parent from scratch, and a dependency change rebuilds both commits, each several minutes longer. A failed comparison reruns the benchmark, and the check fails only when the same metric is past its limit again: under load a different metric lands past its limit now and then, while a real regression repeats. The gate blocks about 100 µs of added latency per request, about 300 µs on SSE time to first byte, and about 25% more CPU per unit of work; the noisiest latency tolerances come from A/A runs ([research.md](research.md#gate-method-the-parent-in-alternating-rounds)). Smaller effects need `scripts/ab.sh <wye-a> <wye-b> [rounds]`, which runs two binaries in alternating rounds. Adopt an optimization only when it wins beyond noise in two A/B runs and worsens no gated metric. Results and rejected experiments are recorded in [research.md](research.md).

## Continuous integration

GitHub Actions run on every pull request and push to `main` (`.github/workflows/`). They add to the commit gate; they do not replace it, because shared runners are too noisy to gate latency or CPU.

| Workflow | Runs | Gates |
|---|---|---|
| `ci.yml` | pushes to `main` and every pull request | `cargo fmt --check`, clippy with `-D warnings`, tests on Linux and macOS, release and `alloc-stats` builds, `cargo deny check` |
| `bench.yml` | pull requests that touch more than docs | `scripts/ci-bench.sh`: the base commit and the pull request in alternating rounds on one runner (`scripts/bench-against.sh`), gating allocation counts and failed requests (`wye-bench compare --only 'alloc_*,*_failed_requests,*_failed_streams' --allow-new`); latency and CPU are reported in the job summary only |
| `docker.yml` | pushes to `main`, and pull requests that touch the image | `scripts/image-smoke.sh` |
| `release.yml` | a `v*` tag | generated by dist ([operations.md](operations.md#distribution)); never edited by hand |

Allocation counts vary under 1% between runs and do not depend on the machine, so comparing a pull request with its base on one runner keeps their gate as strict as the local one without a second baseline. Dependabot opens one grouped Cargo update a week; actions are pinned by commit and updated by hand (`release.yml`'s by upgrading dist and rerunning `dist init`). CI makes no provider calls and needs no secrets.

A release: bump `version` in `crates/wye/Cargo.toml`, commit through the gate, then `git tag v0.1.0 && git push origin v0.1.0`. `dist plan` previews what the tag will build.

## Benchmark

`wye-bench run --wye <path> --out <results.json>` runs each scenario directly against a mock Anthropic upstream and through Wye, and reports what Wye adds:

| Path or variant | Scenarios |
|---|---|
| Plain HTTP/1.1 (no prefix) | unary latency and CPU, SSE time to first byte and per-event delay, CPU per token-sized event (128,000 unpaced 64-byte events), bulk CPU per MB, 1,000 idle streams, cancel release, 1 MiB uploads, the Claude Code-shaped `agent_upload` |
| TLS + HTTP/2 (`tls_`, the production shape) | the same, plus 2,000 token-paced streams and the stress scenarios: 32 requests on cold connections, paced streams during back-to-back 1 MiB uploads (`mixed_`), paced streams beside 200 stalled clients (`stalled_`) |
| `auth_`, `models_`, `diag_`, `otlp_` (plain path) | per-request scenarios; `models_` and `diag_` also uploads |
| `bedrock_` (plain path) | Wye with a Claude route to one Bedrock upstream: every request held until its `model` and its id rewritten with the `anthropic.` prefix; per-request scenarios and allocations |
| `failover_` (plain path) | Wye with a Claude route of two accounts (both the mock) and `failover`: every request body kept for resending until the response; per-request scenarios, uploads, and allocations per request and per agent-shaped upload |
| `refused_` (plain path) | `failover_` whose first account, tried first (`select = "ordered"`), refuses every fourth request with `429` before reading it: per-request scenarios and uploads; failed requests gated (uploads allow 3: one refused after its body was delivered whole is answered as it is) |
| `openai_` (plain path) | Wye on `[routes.openai]`; the mock answers `/v1/responses` with OpenAI Responses JSON and streams (created, text deltas, completed with usage): per-request scenarios and CPU per event, so the Responses scanners are gated |

Failed requests and streams are gated at zero. `--krakend <path>`, `--litellm <path>`, and `--bifrost <path>` run the same scenarios through those gateways ([comparison runs](research.md#comparison-runs)).

`--wye-alloc <path>` adds `alloc_` metrics from a `--features alloc-stats` build, whose global allocator counts every heap allocation and serves the counters at `GET /debug/alloc` (benchmarks only; never ship it): allocations and bytes per request, per streamed event, and per stream; live heap per idle stream, also after a 512 KB request and on TLS, and per stalled stream on TLS; allocations per 1 MiB upload. Counts vary under 1% (uploads ~5%), so the gate catches one extra allocation per request, stream, or event, which CPU timing never resolves.

`wye-bench load (--wye | --wye-openai | --wye-refused | --krakend) <path> --scenario <upload|tls-upload|paced|capacity|events> --seconds N` keeps one scenario running and prints the gateway's pid, for `sample <pid>` (macOS) or `perf` (Linux). `paced` is the soak test: resident memory, and with an alloc-stats build the live heap, after every batch of 2,000 streams. `capacity` runs Wye on one Tokio worker (`WYE_BENCH_GATEWAY_THREADS=N` for more) and steps paced streams up until the added p99 passes 10 ms (`--seconds` is ignored). `events` repeats the gated CPU-per-event scenario (unpaced 64-byte events; `WYE_BENCH_EVENT_BYTES` changes the payload) and prints CPU per event; with `--wye-openai` the events are OpenAI Responses. `--wye-refused` runs the `refused_` variant (uploads to an account that refuses one in four) and prints failed requests.

## Code changes

- Correctness and protocol compatibility first; then no unnecessary allocation, buffering, locks, or blocking on the request path.
- Every change ends with a review pass over the files it touched: names, duplication, dead code, stale comments, module boundaries. Refactor instead of patching when logic repeats, a module does two jobs, or a name no longer fits.
- A refactor changes no behavior, keeps existing tests unmodified, and is committed separately with "No behavior change" in the message.
- Tests are deterministic: local mock upstream, no paid provider calls. Parsers of untrusted input get property tests.
- `unwrap`, `expect`, panics, and indexing are denied outside tests; `unsafe` is forbidden in `gateway-core` and denied elsewhere.
