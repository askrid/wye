# Development

How changes are made, verified, and committed. Wye must never be the bottleneck between a client and the provider, so every commit is checked for performance as well as correctness.

## Layout

| Path | Contents |
|---|---|
| `crates/gateway-core` | Portable policy: config validation, credential verification, model aliases, permissions and output-token ceilings, pricing, upgrade-check rules, request-body and SSE scanners, prompt hashes, agent headers. No I/O, no Tokio. |
| `crates/wye` | The binary: `main.rs`, the offline commands (`cli/`), startup resolution, listener and shutdown (`server.rs`), routing (`gateway.rs`), credentials and permissions, forwarding (`upstream/`), usage log and OTLP export (`usage_log/`). Module map in [architecture.md](architecture.md#repository-and-workspace) |
| `crates/wye-testkit` | Test support: throwaway CA, test signing keys, mock upstream (plain, or TLS with HTTP/2) |
| `crates/wye-bench` | Benchmark and regression gate: mock Anthropic upstream, load client, soak and capacity runs, competitor runs |
| `bench/baseline.json` | Benchmark results recorded on the development laptop: the numbers the docs cite. The gate does not read it |
| `examples/` | The configuration reference (`wye.toml`), a subscription config, and policy-test cases |
| `scripts/check.sh`, `scripts/bench-gate.sh`, `scripts/bench-against.sh` | Commit gate; its benchmark comparison with one rerun; this checkout against a commit in alternating rounds |
| `scripts/ab.sh` | A/B comparison of two binaries |
| `scripts/ci-bench.sh` | The pull-request benchmark: a commit against its base on one machine |
| `scripts/upload-latency.sh` | Upload latency over HTTP/1.1 and HTTP/2 against the real API (needs an API key; not part of the gate) |
| `Dockerfile`, `scripts/image-smoke.sh` | OCI image and its build-and-run smoke test (needs Docker; not part of the gate) |
| `.github/workflows/`, `dist-workspace.toml`, `deny.toml` | CI and release workflows, the release build's settings, the dependency policy |

## Commit gate

`scripts/check.sh` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, all tests, a release build plus an allocation-counting build (`--features alloc-stats`, in `target/alloc`), and the benchmark of this checkout against its parent commit. The pre-commit hook runs it for every commit that touches more than docs (or `docs/policy.md`, which a test reads), and refuses one with unstaged changes or untracked source files, since the check builds the working tree: what is benchmarked must be what is committed. Cargo runs with `--locked`, and clippy also checks the `alloc-stats` build, the only one with `unsafe` code. Enable it once per clone:

```bash
git config core.hooksPath .githooks
```

`scripts/bench-against.sh` builds the parent in a worktree (`target/reference`, kept between checks, so it rebuilds only what the last commit changed) and runs both commits in alternating rounds, three each, each commit with its own `wye-bench`. The gate compares the medians (the worst round, for failure counts); a comparison that fails without naming a metric fails the gate. A stored result would carry the machine's state on the day it was recorded ([why](research.md#gate-method-the-parent-in-alternating-rounds)). Rounds minutes apart see the same machine, so nothing has to be recorded first and the gate runs on any machine.

- **A change that fails the check is not committed**, including any benchmark regression. Fix it or drop it; never weaken a test or a tolerance to pass. Never commit with `--no-verify`.
- `WYE_GATE_ACCEPT=<pattern,...>` lets the named metrics regress (they read `accepted`), only for an intended, reviewed tradeoff, with the before/after numbers and the reason in the commit message: `WYE_GATE_ACCEPT=tls_upload_cpu_ms_per_mb git commit`.
- A metric the parent does not measure yet reads `new`; it is gated from the next commit on.
- Each commit is compared only with its parent, so regressions that each stay under a tolerance can add up. `WYE_GATE_AGAINST=<ref> scripts/check.sh` compares with any commit; run it against the last release before tagging the next. An amend is compared with the commit it replaces unless run with `WYE_GATE_AGAINST=HEAD~1`.
- Refresh `bench/baseline.json` by hand on an idle machine, together with the numbers in the docs: `target/release/wye-bench run --wye target/release/wye --out bench/baseline.json --wye-alloc target/alloc/release/wye --repeat 7`.
- The proxy tests and the benchmark open thousands of sockets. `check.sh` and CI raise the open-file limit to 8192; on macOS, whose default is 256, run `ulimit -n 8192` before a bare `cargo test`. The stalled-client test keeps 256 loopback connections full, and their client ends autotune receive buffers larger than the 96 MB network pool of a 7 GB macOS CI runner, so CI caps that autotuning (`net.inet.tcp.autorcvbufmax`).

A check takes about 8 minutes after an ordinary code change: tests about 15 s, incremental builds of both commits about a minute, and six benchmark runs of about 67 s each (Wye and eight variants on the plain path, then the TLS path; `wye-bench` prints the duration of each phase to stderr). The first check in a checkout builds the parent from scratch, about 90 s more on the laptop; a dependency change rebuilds both commits. A failed comparison reruns the benchmark, and the check fails only when the same metric is past its limit again: under load a different metric lands past its limit now and then, while a real regression repeats. A failed request or stream is never noise and fails the check at once. The gate blocks about 100 µs of added latency per request, about 400 µs on SSE time to first byte (300 µs plus 40% of the parent's value), and about 25% more CPU per unit of work; the noisiest latency tolerances come from A/A runs ([research.md](research.md#gate-method-the-parent-in-alternating-rounds)). Smaller effects need `scripts/ab.sh <wye-a> <wye-b> [rounds]`, which runs two binaries in alternating rounds. Adopt an optimization only when it wins beyond noise in two A/B runs and worsens no gated metric. Results and rejected experiments are recorded in [research.md](research.md).

## Continuous integration

GitHub Actions run on every pull request and push to `main` (`.github/workflows/`). They add to the commit gate without replacing it: shared runners are too noisy to gate latency or CPU.

| Workflow | Runs | Gates |
|---|---|---|
| `ci.yml` | pushes to `main` and every pull request | `cargo fmt --check`, clippy with `-D warnings`, tests on Linux and macOS, release and `alloc-stats` builds, `cargo deny check` |
| `bench.yml` | pull requests that touch more than docs | `scripts/ci-bench.sh`: the base commit and the pull request in alternating rounds on one runner, gating allocation counts, failed requests, and failed streams with the local gate's rerun rule (`scripts/bench-gate.sh`); latency and CPU are reported in the job summary only |
| `docker.yml` | pushes to `main`, and pull requests that touch the image | `scripts/image-smoke.sh` |
| `release.yml` | a `v*` tag | generated by dist ([operations.md](operations.md#distribution)); never edited by hand |

Allocation counts vary under 1% between runs and do not depend on the machine, so their gate on a shared runner is as strict as the local one. Dependabot opens one grouped Cargo update a week; actions are pinned by commit and updated by hand (`release.yml`'s by upgrading dist and rerunning `dist init`). CI makes no provider calls and needs no secrets.

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

Failed requests and streams are gated at zero: a failure count takes the worst of the rounds, not the median, and its limit is absolute, never the parent's count (the `refused_` uploads allow 3). A stream counts as failed when it errors, ends before all its events, or has not ended after 30 s, in every SSE scenario of every variant (`*_sse_failed_streams`), throughput included; a request that takes longer than 30 s fails too, so a hung gateway fails the check instead of hanging it. The CPU phases' failures count too, and the allocation phases refuse to report counts from a run in which anything failed. The cancellation release times have a ceiling of one second whatever the parent reads. `--krakend <path>`, `--litellm <path>`, and `--bifrost <path>` run the same scenarios through those gateways ([comparison runs](research.md#comparison-runs)).

CPU time comes from `ps` on macOS and from `/proc/<pid>/stat` on Linux (whose `ps` rounds to whole seconds), in 10 ms steps either way; each CPU scenario runs long enough to make the step negligible.

`--wye-alloc <path>` adds `alloc_` metrics from a `--features alloc-stats` build, whose global allocator counts every heap allocation and serves the counters at `GET /debug/alloc` (benchmarks only; never ship it): allocations and bytes per request, per streamed event, and per stream; live heap per idle stream, also after a 512 KB request and on TLS, and per stalled stream on TLS; allocations per 1 MiB upload. Counts vary under 1% (uploads about 5%), so the gate catches one extra allocation per request, stream, or event, which CPU timing cannot resolve.

`wye-bench load (--wye | --wye-openai | --wye-refused | --krakend) <path> --scenario <name> --seconds N` keeps one scenario running and prints the gateway's pid, for `sample <pid>` (macOS) or `perf` (Linux):

- `upload`, `tls-upload`: 1 MiB uploads on the plain or the TLS path.
- `paced`: the soak test. Resident memory, and with an alloc-stats build the live heap, after every batch of 2,000 streams.
- `capacity`: Wye on one Tokio worker (`WYE_BENCH_GATEWAY_THREADS=N` for more), stepping paced streams up until the added p99 passes 10 ms; `--seconds` is ignored.
- `events`: the gated CPU-per-event scenario (unpaced 64-byte events; `WYE_BENCH_EVENT_BYTES` changes the payload), printing CPU per event; with `--wye-openai` the events are OpenAI Responses.

`--wye-refused` runs the `refused_` variant (uploads to an account that refuses one in four) and prints failed requests.

## Scripts and benchmark variables

| Variable | Used by | Meaning |
|---|---|---|
| `WYE_GATE_ACCEPT` | `scripts/check.sh` | Metrics an approved tradeoff may regress ([commit gate](#commit-gate)) |
| `WYE_GATE_AGAINST` | `scripts/check.sh` | The commit to compare with, instead of the parent |
| `WYE_BENCH_GATEWAY_LOG` | `wye-bench` | Keep each gateway's output next to its config |
| `WYE_BENCH_GATEWAY_THREADS` | `wye-bench`, `capacity` | Tokio workers (default 1) |
| `WYE_BENCH_EVENT_BYTES` | `wye-bench`, `events` | Event payload size |
| `WYE_BENCH_UPSTREAM` | `wye-bench` | Extra `[upstream]` lines, for tuning experiments |
| `WYE_BENCH_BIFROST_POOLS` | `wye-bench` | `default` keeps Bifrost's default pools |
| `WYE_BENCH_PROVIDER_KEY`, `WYE_BENCH_HASH_KEY` | `wye-bench` | Set for the gateway it starts; not for you to set |
| `WYE_IMAGE`, `WYE_SMOKE_PORT` | `scripts/image-smoke.sh` | The image tag and port |
| `WYE_URL` | `scripts/upload-latency.sh` | Also time the same uploads through this gateway |

The variables `wye` itself reads are in [operations.md](operations.md#environment-variables).

## Code changes

- Correctness and protocol compatibility first; then no unnecessary allocation, buffering, locks, or blocking on the request path.
- Every change ends with a review pass over the files it touched: names, duplication, dead code, stale comments, module boundaries. Refactor instead of patching when logic repeats, a module does two jobs, or a name no longer fits.
- A refactor changes no behavior, keeps existing tests unmodified, and is committed separately with "No behavior change" in the message.
- Tests are deterministic: local mock upstream, no paid provider calls. Parsers of untrusted input get property tests.
- `unwrap`, `expect`, panics, and indexing are denied outside tests; `unsafe` is forbidden in `gateway-core` and denied elsewhere.

## Design principles

How the code is structured, beyond the rules in [Code changes](#code-changes). Each principle names where the code follows it, so a review can check new code against an example.

- **Functional core, imperative shell.** Decisions are pure functions in `gateway-core`, with no I/O, clock, or Tokio: policy and the output-token ceiling, the route choice, pricing, cache-miss classification, upgrade rules, credential checks. `crates/wye` reads sockets, feeds the core bytes and headers, and acts on the answer. Like a sans-I/O protocol library, the core is tested without a socket, and property tests cover its parsers ([Bernhardt](https://www.destroyallsoftware.com/screencasts/catalog/functional-core-imperative-shell), [sans-I/O in Rust](https://www.firezone.dev/blog/sans-io)).
- **One implementation of each decision.** `wye explain` and `wye policy-test` decide through `gateway-core`'s `admission::BodyChecks`, as `serve` does, and `wye doctor` resolves secrets with `startup`'s functions, instead of restating them: an offline answer that can drift from the server's is worse than none. A proxy test sends the same requests to a gateway and to `explain` and requires the same answers. The same holds for vocabularies: one enum per closed set, such as a record's outcome, usage status, and cache-miss reason, written as a string only at the edge (the JSON line, OTLP); and one function, `responses::refuse`, that turns a policy refusal (`admission::Refusal`) into a response in the request's API shape.
- **Parse, don't validate.** Config, credentials, model patterns, and prices become types at the boundary, so later code cannot meet an invalid value; fields that only make sense together live in one type ([King](https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/)).
- **Resources are guards.** Every permit, gauge unit, budget byte, and usage record belongs to a value that releases or writes it on drop (`gauge::Held`, `usage_log::Unanswered`, `FailoverBody`'s hand-back), because dropping a future is how Tokio cancels. A request yields exactly one record because a guard writes it.
- **A bound bounds what it names.** A memory budget counts memory, not payload: a held chunk counts at least `MIN_CHUNK_CHARGE`, since a body sent a byte at a time pins far more than its bytes. A deadline covers every wait it names: the connect timeout covers the TLS handshake too.
- **Deep modules, inward dependencies.** A module hides a decision behind a small interface; a public item used once, or a function that only passes arguments on, is folded back ([Ousterhout](https://web.stanford.edu/~ouster/cgi-bin/book.php)). Dependencies point one way: the CLI uses `gateway-core` and `startup`, never server internals; sibling modules in `crates/wye` form no cycles; `gateway-core` depends on nothing in `crates/wye`.
- **Closed sets are enums, dispatch is static.** `Api`, `Provider`, and `UsageScanner` are enums matched per chunk; a trait needs two implementations; the data path is generic, and `dyn` stays at cold boundaries such as startup.

## Review rounds

The review pass at the end of every change sees only the files it touched. A review round reads the whole repository: after a run of feature work, before a release, and when asked. It covers code, tests, the benchmark, scripts, CI, dependencies, configuration, examples, comments, the README, and the docs.

1. Review by concern, reading each area in full rather than sampling it:
   - **Malformed and adversarial requests:** HTTP framing, methods, paths, headers, credentials, JSON that a policy could read differently from the provider, and the status each case gets.
   - **Stress and misbehaving peers:** slow, endless, or oversized upstream responses; slow or flooding clients; full queues; a stalled collector; shutdown under load. Every buffer stays bounded and every task has an owner.
   - **Text against code:** every default, limit, flag, message, metric, record field, and status a document names, checked at its source in the code. Names removed since the last round (`git log`) must leave no trace. Every relative link and anchor must resolve.
   - **Tests, benchmark, scripts, CI, dependencies:** assertions that cannot fail, test names that promise more than the test checks, real-time waits, gated metrics never produced, and dependencies without `default-features = false`.
   - **Structure:** the code against the [design principles](#design-principles): duplicated decisions, stringly-typed vocabularies, resources released by hand, dependency cycles.
2. A finding counts only after it is traced through the code. A bug gets a test that fails before the fix. After the fix, undo it once and confirm the test fails again (a mutation check), so the test is known to guard the fix.
3. Run `wye report` on a real usage log. A number the report hides or mislabels is a bug like any other.
4. Each fix is its own commit through the gate. Refactors are separate commits and change no behavior.
5. Record each finding that is not fixed, with the reason, where the next round will find it: open work in [roadmap.md](roadmap.md), open questions in [research.md](research.md).
