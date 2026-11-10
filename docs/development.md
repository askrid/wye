# Development

How changes are made, verified, and committed. Wye must never be the bottleneck between a client and the provider, so every commit that changes code is checked for performance as well as correctness.

## Layout

One repository; the four crates are the members of one Cargo workspace.

| Path | Contents |
|---|---|
| `crates/gateway-core` | Decisions with no I/O: config, gateway tokens, model permissions, upstream selection, pricing, scanners |
| `crates/wye` | The binary: server, upstream client, usage log and OTLP, CLI |
| `crates/wye-testkit` | Test CA, mock upstream, signing keys, identity-provider tokens |
| `crates/wye-bench` | The benchmark, alternating rounds and the gate's decision, load and capacity runs, competitor runs |
| `examples/` | `wye.toml` (the config reference); one directory per use (`developer/`, `team/`, `app/`), each a `wye.toml` and, for the last two, its `policy-tests/` |
| `bench/baseline.json` | Wye's metrics from the median of three [reference-environment](benchmarks.md#reference-environment) runs, the numbers the docs cite. The commit gate does not read it |
| `bench/reference/` | The [reference environment](benchmarks.md#reference-environment): CloudFormation templates, `run.sh` and `cleanup.sh` (run by hand), the instance's `remote.sh`, the LiteLLM lock file, and recorded results |
| `scripts/check.sh` | The commit gate |
| `scripts/bench.sh` | Builds, then runs `wye-bench gate` or `alternate`: `gate <ref>` (this checkout against a commit, with one rerun), `ci <ref>` (the pull-request benchmark, gating what no machine changes), `ab <a> <b>` (two binaries, every metric, no verdict) |
| `scripts/machine-lock.sh` | Runs a heavy job under one lock that every worktree shares, so the gate, A/B runs, and site builds take turns on one machine; `check.sh` and `bench.sh` take it themselves, CI skips it |
| `scripts/upload-latency.sh` | Upload latency over HTTP/1.1 and HTTP/2 against the real API (needs an API key; not part of the gate) |
| `Dockerfile`, `scripts/image-smoke.sh` | OCI image and its build-and-run smoke test (needs Docker; not part of the gate) |
| `.github/workflows/`, `dist-workspace.toml`, `deny.toml` | CI and release workflows, the release build's settings, the dependency policy |
| `site/` | The website, built from the repository's own files; its own Cargo workspace and check, `site/check.sh` ([site/README.md](../site/README.md)) |

Because `gateway-core` does no I/O ([functional core](#design-principles)), it could back the planned [WASM adapter](roadmap.md#m6-optional-extensions); add no crate per provider, and no speculative transport interface for that adapter.

## Commit gate

`scripts/check.sh` runs these steps in order, with Cargo's `--locked`:

1. `cargo fmt --check`.
2. `cargo clippy --all-targets -- -D warnings`, which also checks the `alloc-stats` build, the only one with `unsafe` code.
3. `cargo doc` with `-D warnings`, over private items too.
4. All tests.
5. A release build plus an allocation-counting build (`--features alloc-stats`, in `target/alloc`).
6. The benchmark of this checkout against its parent commit.

The pre-commit hook runs it for every commit that touches more than docs, `bench/baseline.json`, and the website, `site/` and its workflow `.github/workflows/site.yml` (or `docs/policy.md`, which a test reads), and refuses one with unstaged changes or untracked source files, since the check builds the working tree. What is benchmarked must be what is committed. A docs-only commit that changes `README.md` or `docs/guide.md` runs the one test that reads their install command. A commit that changes `site/`, its workflow, a file the site reads (`README.md`, `CHANGELOG.md`, `docs/operations.md`, `docs/benchmarks.md`, the manifests), or one its check uses (`clippy.toml`, `rust-toolchain.toml`, `scripts/machine-lock.sh`), runs `site/check.sh` instead or as well; it takes seconds once the site is built. Enable the hook once per clone:

```bash
git config core.hooksPath .githooks
```

`scripts/bench.sh gate` builds the parent in a worktree (`target/reference`, kept between checks), and `wye-bench gate` runs both commits in alternating rounds, three each, each with its own `wye-bench`. The gate compares the medians (the worst round, for failure counts) under both commits' tolerance lists, so a commit cannot pass by loosening the gate it is judged by; a comparison that fails without naming a metric fails the gate.

A failed comparison reruns the benchmark, and the check fails only when the same metric is past its limit again. Load pushes a different metric past its limit now and then, while a real regression repeats ([evidence](benchmarks.md#gate-tolerances)). A failed request or stream is never noise and fails the check at once.

The gate blocks added latency per request beyond the parent's plus 20% and 40 µs (plain path), SSE time to first byte beyond the parent's plus 40% and 300 µs, and CPU per unit of work beyond the parent's plus 12-25% and a small floor ([tolerances](benchmarks.md#gate-tolerances)). Smaller effects need `scripts/bench.sh ab <wye-a> <wye-b> [rounds]`. Adopt an optimization only when it wins beyond noise in two A/B runs and worsens no gated metric, and record the result in [research.md](research.md#performance-techniques).

- A change that fails the check is not committed, including any benchmark regression. Fix it or drop it; never weaken a test or a tolerance to pass. Never commit with `--no-verify`.
- `WYE_GATE_ACCEPT=<pattern,...>` lets the named metrics regress (they read `accepted`), only for an intended, reviewed tradeoff, with the before/after numbers and the reason in the commit message, as in `WYE_GATE_ACCEPT=tls_upload_cpu_ms_per_mb git commit`.
- A series of commits that leaves the request path's work unchanged (renames, messages, CLI output, tests, docs alongside code) may benchmark once. Each commit but the last runs `WYE_GATE_BENCH=series git commit`, which still runs every step but the release builds and the benchmark, and before any of the series is pushed its tip passes `WYE_GATE_AGAINST=<the series' base> scripts/check.sh`. A change meant to alter performance, or one in doubt, is benchmarked on its own commit.
- A metric the parent does not measure yet reads `new`, unless it is a failure count or a release time past its fixed limit; it is gated from the next commit on.
- Each commit is compared only with its parent, so regressions that each stay under a tolerance can add up. `WYE_GATE_AGAINST=<ref> scripts/check.sh` compares with any commit; run it against the last release before tagging the next. An amend is compared with the commit it replaces unless run with `WYE_GATE_AGAINST=HEAD~1`.
- Refresh `bench/baseline.json` together with the numbers in the docs, from a [reference-environment](benchmarks.md#reference-environment) run on three hosts (`--runs 3`). `wye-bench median` merges the runs into `median.json`, and the baseline keeps its Wye metrics, with `repeat` 9 (three hosts, three repeats each). Run `jq '.metrics |= with_entries(select(.key | test("^(tls_)?(bifrost|agentgateway|litellm|krakend)_") | not)) | .repeat = 9' <dir>/median.json > bench/baseline.json`.
- The `proxy` tests and the benchmark open thousands of sockets. `check.sh` and CI raise the open-file limit to 8192; on macOS, whose default is 256, run `ulimit -n 8192` before a bare `cargo test`. The stalled-client test keeps 256 loopback connections full, and their client ends autotune receive buffers larger than the 96 MB network pool of a 7 GB macOS CI runner, so CI caps that autotuning (`net.inet.tcp.autorcvbufmax`).

A check takes about 8 minutes after an ordinary code change. Tests take about 15 s, incremental builds of both commits about a minute, and six benchmark runs about 67 s each (`wye-bench` prints each phase's duration to stderr). The first check in a checkout takes about 90 s more; a dependency change rebuilds both commits.

The parent runs in alternating rounds because a stored result carries the machine's state on the day it was recorded ([evidence](benchmarks.md#gate-tolerances)), while the two commits share the machine's state of the moment, and with nothing recorded first the gate runs on any machine. The method is the randomized multiple interleaved trials of [Abedi and Brecht (ICPE 2017)](https://research.spec.org/icpe_proceedings/2017/proceedings/p287.pdf), with a fixed alternation in place of a random order, at the price of a second build and twice the benchmark time. Rejected alternatives:

- Both commits at once ([duet benchmarking](https://arxiv.org/abs/2001.05811)): the paced and stress scenarios load every core, so two runs at once would measure each other.
- A cloud VM as the gate machine: instances of one type differ between hosts and over time, and a VM cannot fix the clock ([Leitner and Cito](https://arxiv.org/abs/1411.2429), [Laaber et al.](https://peerj.com/preprints/3507v1.pdf)). The [reference environment](benchmarks.md#reference-environment) is one, for published numbers only; it gates nothing.
- The parent's results from its own gate run: halves the benchmark but brings the time gap back.
- A fixed reference commit: catches regressions that add up across commits, but a metric the benchmark adds stays ungated until the reference moves, and each move needs approval. `WYE_GATE_AGAINST` runs that check on demand (above).

## Continuous integration

GitHub Actions run on every pull request and push to `main` (`.github/workflows/`). They add to the commit gate without replacing it. Shared runners are too noisy to gate latency or CPU. CI makes no provider calls and needs no secrets.

- **`ci.yml`**, on pushes to `main` and pull requests that touch more than the website: `cargo fmt --check`, clippy and rustdoc with `-D warnings`, tests on Linux and macOS, release and `alloc-stats` builds, `cargo deny check`.
- **`bench.yml`**, on pull requests that touch more than docs and the website: `scripts/bench.sh ci`, the base commit and the pull request in alternating rounds on one runner, gating allocation counts, failed requests, and failed streams with the local gate's rerun rule. Latency and CPU are reported in the job summary only. Allocation counts barely vary between runs ([their spread](benchmarks.md#current-harness-and-baseline)), so their gate on a shared runner is as strict as the local one.
- **`docker.yml`**, on pushes to `main`, and pull requests that touch the image: `scripts/image-smoke.sh`.
- **`site.yml`**, on pushes to `main` and pull requests that touch the website or a file it reads: `site/check.sh` and `cargo deny check` for the site's workspace; from `main`, a deploy of `site/out` to GitHub Pages.
- **`release.yml`**, on a version tag (`v0.1.0`, `v0.1.0-rc.1`), and `dist plan` on every pull request: generated by [dist](https://github.com/axodotdev/cargo-dist) from `dist-workspace.toml`; edit that file and run `dist generate`, never `release.yml` by hand. Each target builds on a native runner with the benchmarked `release` profile; [operations.md](operations.md#distribution) lists what a release publishes. Beside the builds it calls `release-checks.yml` (read-only), which waits for the newest `ci.yml` run on a push to `main` of the tagged commit and fails unless it passed; the release is created only if that check succeeds.
- **`advisories.yml`**, weekly and on demand: `cargo deny check advisories` for the gateway's workspace and the website's, so an advisory against a locked dependency shows up even when nothing is pushed.

Dependabot opens one grouped Cargo update for the gateway, one for the website, and one for the Dockerfile's base images (pinned by tag and digest) a week. Every action is pinned by commit SHA, with its version in a comment, and updated by hand, `release.yml`'s through `github-action-commits` in `dist-workspace.toml` and `dist generate`. Three things there stay as dist 0.32 generates them, with no option to change them:

- `plan` and each native build install dist with `curl … cargo-dist-installer.sh | sh` from dist's release URL for the pinned version, without a checksum.
- The workflow-level permission is `contents: write`, which every job but the native builds and the CI check inherits, `plan` on pull requests included.
- Plan jobs (`plan-jobs`) do not hold back the release, which is why the CI check is a local-artifacts job.

### Releases

1. Bump `version` in `crates/wye/Cargo.toml` and the `--tag` of the install command in the README's Quick start and [guide.md](guide.md#installing).
2. Commit through the gate and push to `main`.
3. `git tag v0.1.0 && git push origin v0.1.0`.

The tag publishes only once `ci` has passed on that commit (it waits for a run in progress); if `ci` failed, rerun it and then the release's failed jobs, or fix it and tag the fix as the next version. `dist plan` previews what the tag will build.

## Benchmark

`wye-bench run --wye <path> --out <results.json>` runs the scenarios ([method](benchmarks.md#method)) on two upstream paths:

- **Plain HTTP/1.1** (no prefix): unary latency and CPU, SSE time to first byte and per-event delay, CPU per token-sized event (128,000 unpaced 64-byte events), bulk CPU per MB, 1,000 idle streams, cancel release, 1 MiB uploads, the Claude Code-shaped `agent_upload`.
- **TLS + HTTP/2** (`tls_`, the production shape): the same except the Claude Code-shaped `agent_upload`, plus 2,000 token-paced streams and the stress scenarios of 32 requests on cold connections, paced streams during back-to-back 1 MiB uploads (`mixed_`), and paced streams beside 200 stalled clients (`stalled_`).

The variants, all on the plain path:

- **`auth_`, `models_`, `diag_`, `otlp_`**: per-request scenarios; `models_` and `diag_` also uploads.
- **`bedrock_`**: Wye with a Claude route to one Bedrock upstream, with every request held until its `model` is read ([the hold](glossary.md#requests-and-where-they-go)) and its id rewritten with the `anthropic.` prefix; per-request scenarios and allocations.
- **`failover_`**: Wye with a Claude route of two upstreams (both the mock), which fails over, with every request body kept for resending until the response; per-request scenarios, uploads, and allocations per request and per agent-shaped upload.
- **`refused_`**: `failover_` with `standby = true` on the second upstream, whose first upstream rejects every fourth request with `429` before reading it; per-request scenarios and uploads; failed requests gated (uploads allow 3; one rejected after its body was delivered whole is answered as it is).
- **`openai_`**: Wye with one OpenAI upstream; the mock answers `/v1/responses` with OpenAI Responses JSON and streams (created, text deltas, completed with usage); per-request scenarios and CPU per event, so the Responses scanners are gated.

Failed requests and streams are gated at zero:

- A failure count's limit is absolute, never the parent's count.
- A stream counts as failed when it errors, ends before all its events, or has not ended after 30 s, in every SSE scenario of every variant (`*_sse_failed_streams`), throughput included.
- A request that takes longer than 30 s fails too, so a hung gateway fails the check instead of hanging it.
- The CPU phases' failures count too, and the allocation phases refuse to report counts from a run in which anything failed.
- Cancellation release times have a fixed limit of one second, whatever the parent reads.

`--agentgateway <path>`, `--bifrost <path>`, `--litellm <path>`, and `--krakend <path>` run the same scenarios through those gateways ([comparison runs](benchmarks.md#comparison-runs)).

Two subcommands run several builds in alternating rounds, each side through its own `wye-bench`, as a child process:

- `wye-bench alternate --side <name>=<wye-bench>,<wye>[,<alloc-stats wye>] ... --rounds N --repeat R --out <dir>`: each side's `run --repeat R` once per round, the order reversed every other round, then `<dir>/<name>.json`, the median of its rounds (failure counts: the worst). `scripts/bench.sh ab` is this and `wye-bench diff`.
- `wye-bench gate --reference <checkout> --current <checkout> --out <dir> [compare flags]`: three rounds of each checkout's `target/release` and `target/alloc/release` builds, compared by both commits' `wye-bench compare`, with the rerun rule of the [commit gate](#commit-gate).

CPU time comes from `ps` on macOS and from `/proc/<pid>/stat` on Linux (whose `ps` rounds to whole seconds), in 10 ms steps either way; each CPU scenario runs long enough to make the step negligible.

`--wye-alloc <path>` adds `alloc_` metrics from a `--features alloc-stats` build, whose global allocator counts every heap allocation and serves the counters at `GET /debug/alloc` (benchmarks only; never ship it). They cover allocations and bytes per request, per streamed event, and per stream; live heap per idle stream, also after a 512 KB request and on TLS, and per stalled stream on TLS; allocations per 1 MiB upload. Counts barely vary ([spread](benchmarks.md#current-harness-and-baseline)), so the gate catches one extra allocation per request, stream, or event, which CPU timing cannot resolve.

`wye-bench load (--wye | --wye-openai | --wye-refused | --krakend) <path> --scenario <name> --seconds N` keeps one scenario running and prints the gateway's pid, for `sample <pid>` (macOS) or `perf` (Linux); `--wye-refused` runs the `refused_` variant and prints failed requests:

- `upload`, `tls-upload`: 1 MiB uploads on the plain or the TLS path.
- `paced`: the soak test. Resident memory, and with an alloc-stats build the live heap, after every batch of 2,000 streams.
- `capacity`: Wye on one Tokio worker (`WYE_BENCH_GATEWAY_THREADS=N` for more), stepping paced streams up until streams fail, the worker saturates, or the added p99 passes 100 ms, and reporting the largest step within 10 ms; `--seconds` is ignored.
- `events`: the gated CPU-per-event scenario (unpaced 64-byte events; `WYE_BENCH_EVENT_BYTES` changes the payload), printing CPU per event; with `--wye-openai` the events are OpenAI Responses.

## Scripts and benchmark variables

| Variable | Used by | Meaning |
|---|---|---|
| `WYE_GATE_ACCEPT` | `scripts/check.sh` | Metrics an approved tradeoff may regress ([commit gate](#commit-gate)) |
| `WYE_GATE_AGAINST` | `scripts/check.sh` | The commit to compare with, instead of the parent |
| `WYE_BENCH_GATEWAY_LOG` | `wye-bench` | Keep each gateway's output next to its config |
| `WYE_BENCH_GATEWAY_THREADS` | `wye-bench`, `capacity` | Tokio workers (default 1) |
| `WYE_BENCH_GATEWAY_CPUS` | `wye-bench` | Linux only: a cpu list (`0-3,8-11`) every gateway runs on, through `taskset`; LiteLLM gets one worker per CPU in it |
| `WYE_BENCH_EVENT_BYTES` | `wye-bench`, `events` | Event payload size |
| `WYE_BENCH_UPSTREAM` | `wye-bench` | Extra lines in the upstream entry, for tuning experiments |
| `WYE_BENCH_BIFROST_POOLS` | `wye-bench` | `default` keeps Bifrost's default pools |
| `WYE_BENCH_PROVIDER_KEY`, `WYE_BENCH_HASH_KEY` | `wye-bench` | Set for the gateway it starts; not for you to set |
| `WYE_IMAGE`, `WYE_SMOKE_PORT` | `scripts/image-smoke.sh` | The image tag and port |
| `WYE_URL` | `scripts/upload-latency.sh` | Also time the same uploads through this gateway |

The variables `wye` itself reads are in [operations.md](operations.md#environment-variables).

## Writing code

Correctness and protocol compatibility come first; then no unnecessary allocation, buffering, locks, or blocking on the request path.

### Design principles

Each principle names where the code follows it, so a review can check new code against an example.

- **Functional core, imperative shell.** Decisions (model permissions and the output-token ceiling, upstream selection, pricing, cache-miss classification, upgrade rules, gateway-token checks) are pure functions in `gateway-core`, with no I/O (sockets, files, the environment), clock, or Tokio. `crates/wye` reads sockets, feeds the core bytes and headers, and acts on the answer. Like a sans-I/O protocol library, the core is tested without a socket, and property tests cover its parsers ([Bernhardt](https://www.destroyallsoftware.com/screencasts/catalog/functional-core-imperative-shell), [sans-I/O in Rust](https://www.firezone.dev/blog/sans-io)).
- **One implementation of each decision.** `wye explain` and `wye policy-test` decide through `gateway-core`'s `admission::BodyChecks`, as `serve` does, and `wye doctor` resolves secrets with `startup`'s functions, instead of restating them. An offline answer that can drift from the gateway's is worse than none. A `proxy` test sends the same requests to a gateway and to `explain` and requires the same answers. The same holds for:
  - vocabularies: one enum per closed set, such as a record's outcome, usage status, and cache-miss reason, written as a string only at the edge (the JSON line, OTLP);
  - refusals: one function, `responses::refuse`, turns a policy refusal (`admission::Refusal`) into a response in the request's API shape.
- **Parse, don't validate.** Config, tokens, model patterns, and prices become types at the boundary, so later code cannot meet an invalid value; fields that only make sense together live in one type ([King](https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/)).
- **Resources are guards.** Every permit, gauge unit, budget byte, and usage record belongs to a value that releases or writes it on drop (`gauge::Reserved`, `usage_log::Unanswered`, `FailoverBody`'s hand-back), because dropping a future is how Tokio cancels. A request yields exactly one record because a guard writes it.
- **A bound bounds what it names.** A memory budget counts memory, not payload. A held chunk counts at least `MIN_CHUNK_CHARGE`, since a body sent a byte at a time pins far more than its bytes. A deadline covers every wait it names. The connect timeout covers the TLS handshake too.
- **Deep modules, inward dependencies.** A module hides a decision behind a small interface; a public item used once, or a function that only passes arguments on, is folded back ([Ousterhout](https://web.stanford.edu/~ouster/cgi-bin/book.php)). Dependencies point one way. The CLI uses `gateway-core` and `startup`, never the request path's internals; sibling modules in `crates/wye` form no cycles; `gateway-core` depends on nothing in `crates/wye`.
- **Closed sets are enums, dispatch is static.** `Api`, `Provider`, and `UsageScanner` are enums matched per chunk; a trait needs two implementations; the data path is generic, and `dyn` stays at cold boundaries such as startup.

### Rust conventions

The rules the design principles do not already state.

#### Toolchain and style

- Stable Rust, edition 2024, pinned in `rust-toolchain.toml`; lints and dependency versions are set once in the workspace's `Cargo.toml`.
- Names follow the Rust API Guidelines; a module with children is `foo.rs` plus `foo/`, never `mod.rs`.
- Every module has a `//!` doc and every public item a `///` doc (rustdoc runs with `-D warnings`); comments say why, not what.
- A doc's first line is one sentence saying what the item is or holds, not how: a noun phrase for a type, `Whether ...` for a `bool`, and the `None` or `Err` case by the second sentence. A module doc names what the module decides, then its invariants and their reasons. A doc never restates the signature; config keys and wire names go in backticks, terms are the [glossary](glossary.md)'s, and comment lines wrap at 80 columns.
- One way to do each thing: extend the existing pattern (one HTTPS client setup, one config snapshot) before adding a second.

#### Types and errors

- Errors are typed enums per concern (`ConfigError`, `TokenError`, `PolicyError`, ...); the CLI turns them into messages. No `anyhow`.
- Identifiers and units get their own types. Money is an integer, pico-USD (`pricing::Pico`), never floating point; arithmetic that would overflow makes a cost `unknown`, never wrong.
- A secret's type redacts it in `Debug` and never implements `Serialize` (`UpstreamAuth`, `HashKey`).
- No panic on the request path: `unwrap`, `expect`, `panic!`, and indexing are denied by lint outside tests (`wye-bench` and `wye-testkit` allow some), and each connection runs in its own task, so a panic cannot reach another stream.

#### The data path

- Bodies are `bytes::Bytes` frames on streaming bodies, relayed without copying, a streaming response never collected.
- Startup resolves all it can, such as upstream URIs, header values, and price tables. No `format!` or `String` building per request where a precomputed value works.
- Request bodies are read by hand-written scanners over `memchr`, for the few fields policy needs; small JSON objects (gateway-token claims, a usage object) are parsed with borrowing serde types (`RawValue`, `IgnoredAny`). A forwarded body is never re-serialized; a rewrite splices the new value in place.
- Every buffer and queue has a fixed bound, a `DEFAULT_*` in `config.rs` where operators tune it, one derived from those (`Server::max_total_held_bytes`), otherwise a constant listed under [Fixed limits](architecture.md#fixed-limits).

#### Concurrency

- Tokio's multi-thread runtime. A worker never blocks; it does no blocking I/O, and CPU work longer than about 100 µs goes to `spawn_blocking`.
- No locks on the request path; shared state is immutable behind `Arc`. Where a lock is unavoidable, a synchronous `Mutex`, never held across `.await` (denied by lint).
- Every spawned task has an owner that awaits or aborts it, a `JoinSet` or a `JoinHandle` its starter keeps. No detached tasks.
- Backpressure comes from polling the body; a channel, where one is needed, is bounded. Usage records never slow the data path. A full queue drops and counts.
- Every `select!` documents its branches' cancel safety. `Relaxed` atomics are for statistics counters only; any other ordering says why.

#### Unsafe and dependencies

- `unsafe` is forbidden in `gateway-core` and denied elsewhere. The one exception, `wye/src/alloc_stats.rs` (a `GlobalAlloc`, built only with `--features alloc-stats`), carries a `// SAFETY:` proof per block.
- Each dependency is justified in its commit, maintained, and added with `default-features = false`; prefer what Tokio and Hyper already pull in. `cargo deny` runs in CI.

#### Performance work

- Measure first: `wye-bench` for the system, `scripts/bench.sh ab` for effects below the gate's sensitivity, the `alloc_` metrics for heap allocations, and sampled stacks (`sample`, `perf`) for where CPU and waits go.
- A streamed event allocates almost nothing. `alloc_sse_per_event` is gated below 0.05 and reads 0.01 in the [reference environment](benchmarks.md#reference-environment).
- Low-level optimization (`#[inline]`, SIMD, `unsafe`, custom allocators, release-profile settings) lands only with a before and after, adopted by the gate's [A/B rule](#commit-gate). Hot-path work the gate does not measure states its own measurement in the commit message.

#### Tests

- Deterministic: local mock upstreams, `tokio::time::pause` for timeouts, no paid provider calls.
- Property tests (proptest) for the parsers of untrusted input, SSE chunk splits, UTF-8 boundaries, and cost arithmetic; `loom` for any custom synchronization (there is none today). Fuzz targets for those parsers are an [open finding](roadmap.md#open-findings).

### Command-line conventions

Every command follows these, so one learned is all learned. A new flag or command that cannot follow one changes the rule here first.

- **Streams first.** A command's result goes to stdout, and messages about running it (progress, warnings, errors) to stderr, never mixed. Input is a file argument, or `-` for stdin. Wye leaves filtering, compressing, merging, and shipping to pipes (`jq`, `gzcat`, `cat a b |`), so it has no `--filter`, `--gzip`, `--json`, or `--tee`.
- **What a command acts on is an argument; how it acts is a flag.** `wye explain request.json`, `wye report usage.jsonl`, `wye policy-diff old.toml new.toml`, `wye probe <url>`; `--since`, `--ttl`, `--url`.
- **One flag, one meaning, everywhere,** with the same default in every command that takes it:
  - `-c, --config <path>`: the config the command reads, or with `init` creates. Default `$WYE_CONFIG`, else `$WYE_HOME/wye.toml`.
  - `--out <path>`: the one file a command creates that holds a secret (a signing key, a gateway token), created readable only by you and never over an existing file. A redirect cannot promise either.
  - `--token-file <path>`: the gateway token a client presents, read from a file. Default `$WYE_TOKEN`.
  - `--url <url>`: the gateway a client uses. Default `$WYE_URL`, else `http://127.0.0.1:8080`.
  - A duration is a whole number and one unit, `30s`, `10m`, `8h`, `7d`, on the command line (`--ttl`, `--since`) and in the config (`drain_delay = "10s"`), parsed by one function, `gateway_core::duration::parse`; a date is UTC `YYYY-MM-DD`.
- **Wye owns a file only when a redirect cannot do the job,** to create it safely (`--out`), or to act on it over time (`serve --usage-log`, which rotates it). Otherwise, redirect stdout.
- **Secrets never in a flag's value.** A command line is visible to other users in `ps`. A token or key comes from a file or the environment, never `--token <value>`.
- **An environment variable is a fact about the environment** (where the config and your files are, which gateway, which token), and its flag always beats it. Everything else is a flag only. `wye --help` lists every variable a user sets; the rest (`WYE_CLAUDE_BIN`, `WYE_CODEX_BIN`) are documented in [operations.md](operations.md#commands).
- **No arguments do the default,** or fail with one line naming the next command to run.
- **A command that adds or removes something outside its own new files** (a service definition) shows what it will change and asks first; `-y, --yes` skips the question and `--dry-run` only shows. `service restart` does not ask. It reloads what `install` was approved to run. Commands that only create new files (`init`, `token keygen`, `token issue --out`) do not ask. They never overwrite.
- **Exit codes:** `0` done; `1` failed, an error or a check that found problems (`doctor`, `policy-test`, `probe`); `2` wrong usage; `3` done, with something a script should act on (`explain`'s request would be refused, `policy-diff` found widened access, `service status` found nothing installed).
- **Colour only on a terminal,** and only where it carries meaning; never with `NO_COLOR`, `TERM=dumb`, or a pipe.

### Refactoring

Structure is kept up continuously, not in cleanup projects.

- Every change ends with a review pass over the files it touched (names, duplication, dead code, stale comments, module boundaries). Search for leftovers after a rename.
- Refactor instead of patching when logic repeats, a module does two jobs, a name no longer fits, or a file grows past about 400 lines without a clear reason. Refactor toward the code that exists, never toward a feature that does not.
- A refactor changes no behavior, keeps existing tests unmodified, and is committed separately with "No behavior change" in the message.

## Documentation

These docs and the README are the source of truth for design, status, and results; [docs/README.md](README.md) names the one document that owns each concern.

- One fact, one owner: every other document links to the owner instead of restating it, so documents cannot contradict each other.
- A change updates the docs it affects in the same commit (status in [roadmap.md](roadmap.md), the config reference in [examples/wye.toml](../examples/wye.toml), structure in [architecture.md](architecture.md), numbers in [benchmarks.md](benchmarks.md), experiments in [research.md](research.md)).
- Unbuilt design is planned work in roadmap.md or an idea in research.md; a user document (the README, the guide, operations) describes what is built. Where a design document describes what is not, it labels it planned or proposal (not yet implemented; the shape may change), or research (an idea to investigate, not a product promise).
- Finished work leaves research.md. Its conclusion becomes one line in the owning document, and the history stays in git.
- No performance claim without published, reproducible benchmarks; never present an estimate or a lab result as a production measurement.
- Support is declared per endpoint × provider × feature. Do not imply more.
- One name per concept, as [glossary.md](glossary.md) defines it; a new concept gets its entry there first.
- One line per paragraph and per list item (editors and GitHub wrap them). Tables hold short values; prose goes in lists, so GitHub does not squeeze it into narrow columns. Code lines stay under 90 characters.
- After editing docs, check that every relative link and anchor resolves.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/), `<type>(<scope>): <summary>`, then a body that says why and keeps the evidence (before/after numbers, the trade-off, what was rejected).

- **Type**, one per commit: `feat`, `fix`, `perf` (measured), `refactor` ([no behavior change](#refactoring)), `test`, `docs`, `build`, `ci`, `chore`, `revert`.
- **Summary**: imperative, lowercase, no period, about 50 characters; the subject line is never over 72.
- **Scope**, from this list; omitted only when a change spans the repository:

| Scope | Covers |
|---|---|
| `gateway` | The request path in `crates/wye`: the listener, connections, forwarding, streaming, request bodies, TLS, timeouts, shutdown |
| `routing` | Routes and their upstreams, upstream selection, cache affinity, failover, providers |
| `policy` | Gateway tokens, identity providers, model permissions, output-token ceilings, aliases, model discovery |
| `usage` | Usage scanners and records, cost and the price table, cache diagnostics |
| `telemetry` | OTLP logs, metrics, and traces |
| `upgrade` | Upgrade checks |
| `config` | The config schema, its validation, `examples/wye.toml`, secret resolution |
| `cli` | Commands and the agent wrappers |
| `bench` | `wye-bench`, scenarios, competitors, `bench/baseline.json` |
| `gate` | `scripts/check.sh`, `scripts/bench.sh`, the hooks, tolerances |
| `testkit` | Shared test harnesses |
| `site` | The website |
| `release` | The release pipeline and the container image |
| `deps` | Dependency updates (`build(deps)`) |
| `readme`, `changelog` | Documentation commits to `README.md` or `CHANGELOG.md` alone |

A documentation commit about one area takes that area's scope (`docs(policy)`).

## Review rounds

The review pass at the end of every change sees only the files it touched. A review round reads the whole repository after a run of feature work, before a release, and when asked. It covers code, tests, the benchmark, scripts, CI, dependencies, configuration, examples, comments, the README, and the docs.

1. Review by concern, reading each area in full rather than sampling it:
   - **Malformed and adversarial requests:** HTTP framing, methods, paths, headers, credentials, JSON that a policy could read differently from the provider, and the status each case gets.
   - **Stress and misbehaving peers:** slow, endless, or oversized upstream responses; slow or flooding clients; full queues; a stalled collector; shutdown under load. Every buffer stays bounded and every task has an owner.
   - **Text against code:** every default, limit, flag, message, metric, record field, and status a document names, checked at its source in the code. Names removed since the last round (`git log`) must leave no trace. Every relative link and anchor must resolve.
   - **Tests, benchmark, scripts, CI, dependencies:** assertions that cannot fail, test names that promise more than the test checks, real-time waits, gated metrics never produced, and dependencies without `default-features = false`.
   - **Structure:** the code against the [design principles](#design-principles) (duplicated decisions, stringly-typed vocabularies, resources released by hand, dependency cycles).
2. A finding counts only after it is traced through the code. A bug gets a test that fails before the fix. After the fix, undo it once and confirm the test fails again (a mutation check), so the test is known to guard the fix.
3. Run `wye report` on a real usage log. A number the report hides or mislabels is a bug like any other.
4. Each fix is its own commit through the gate; [refactors](#refactoring) are separate commits.
5. Record each finding that is not fixed, with the reason, where the next round will find it (open work in [roadmap.md](roadmap.md), open questions in [research.md](research.md)).
