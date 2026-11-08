# Benchmarks

Wye's [overhead](glossary.md#usage-and-cost) beside four other gateways, all measured on the same cloud host in the same session, and the one command that reproduces it. These are lab results against a mock provider, not production measurements.

## Results

Three runs of the [reference environment](#reference-environment) measured commit `7a83fae` on 2026-11-05, each on a fresh AWS `c7i.4xlarge` (Intel Xeon Platinum 8488C), every gateway on the same two physical cores. Each cell is the median of the three hosts with their range in parentheses; each host's figure is the median of three repeats, or for failures the worst. The upstream path is TLS + HTTP/2; the [raw results](../bench/reference/results/2026-11-05-7a83fae/) hold every metric of both paths.

| TLS + HTTP/2 | Wye | Bifrost | agentgateway | LiteLLM | KrakenD |
|---|---|---|---|---|---|
| Added latency per request, p50 | 325 µs (303-330) | 3.2 ms (2.9-3.3) | 912 µs (835-934) | 107 ms (100-118) | 461 µs (411-468) |
| Added latency per request, p99 | 628 µs (606-640) | 14.1 ms (12.1-14.3) | 1.7 ms (1.7-1.8) | 185 ms (162-192) | 2.9 ms (2.6-3.0) |
| Added time to first byte, p50 | 43 µs (15-92) | 2.5 ms (2.2-2.7) | 497 µs (427-556) | 14.3 ms (12.6-16.9) | 44.5 ms (44.4-44.8) |
| Delay per event, 2,000 paced streams, p50 | 77 µs (50-82) | 8.4 ms (1.9-12.1) | 109 µs (108-217) | 10.9 ms (8.6-12.4) | 209 ms (194-209) |
| Delay per event, 2,000 paced streams, p99 | 282 µs (230-367) | 132 ms (94-145) | 1.0 ms (0.7-1.1) | 490 ms (490-493) | 404 ms (398-405) |
| CPU per request | 62.5 µs (58-63) | 506 µs (452-511) | 118 µs (108-121) | 13.8 ms (12.8-15.0) | 105 µs (95-107) |
| CPU per streamed event (paced) | 14 µs (12-15) | 58 µs (53-61) | 17 µs (15-19) | 140 µs (126-159) | 20 µs (16-20) |
| Memory at idle | 28 MB (28-29) | 358 MB (327-359) | 61 MB (59-61) | 2.14 GB (2.14-2.15) | not measured |
| Memory after load | 194 MB (190-197) | 5.4 GB (5.4-5.5) | 405 MB (397-411) | 3.7 GB (3.5-3.8) | 279 MB (238-303) |
| Failed requests and streams under stress | 0 | 0 | 0 | 104-132 | not run |

How to read it:

- Added is the gateway's percentile minus the direct run's (the same client, mock, payload, and host, in the same session), so it includes everything the gateway does. A p99 can read below its p50.
- A request is a 512-byte unary reply, 32 in flight. Paced streams open over 2 s, each 100 events of 100 bytes 20 ms apart (token pace). CPU and memory are the gateway's process tree (LiteLLM's workers summed); memory is RSS, and after load means after every scenario the gateway ran.
- KrakenD CE buffers each response, so its stream rows measure buffering. An event reaches the client when the whole stream has.
- The stress scenarios and their failures are under [failures](#failures).

## What they claim, what I measured

Each gateway publishes a speed claim. Each is quoted here with its stated conditions, beside the number this harness measured for the same thing. Added latency here is the whole difference between going through the gateway and going direct (same host, same payload, same session, all of the gateway's work included).

### Bifrost

- Claim: "In sustained 5,000 RPS benchmarks, the gateway added only 11 µs of overhead per request." ([README](https://github.com/maximhq/bifrost)). Its [benchmark page](https://docs.getbifrost.ai/benchmarking/t3.xl) footnotes that figure: "Excludes JSON marshalling and HTTP calls, which are required in any implementation". Conditions: a burstable t3.xlarge (4 vCPUs), 5,000 requests per second, 10.32 KB responses; no version, date, or raw data.
- Measured: Bifrost 2.3.2 added 3.2 ms at p50 and 14.1 ms at p99 ([results](#results)), with 32 requests in flight on 4 vCPUs. That figure includes the JSON handling and the HTTP call its 11 µs leaves out.
- The same overhead appears as 11 µs, under 15 µs, 20 µs, under 100 µs, and under 200 µs across its pages; its own [t3.medium run](https://docs.getbifrost.ai/benchmarking/t3.medium) reads 59 µs, with a different response size.
- Claim, counting parsing and middleware: "Bifrost measured 0.99ms gateway overhead and LiteLLM measured 40ms gateway overhead with a 60ms mock OpenAI upstream." ([benchmarks](https://getmaxim.ai/resources/benchmarks)). Measured: 3.2 ms against LiteLLM's 107 ms.
- Claim: "50x faster than LiteLLM (P99 latency)" ([product page](https://getmaxim.ai/bifrost); its tiles say 54x). Measured: 13× at p99 (14.1 against 185 ms). The direction holds; the factor does not.

### LiteLLM

- Claim: "8ms P95 latency at 1k RPS" ([README](https://github.com/BerriAI/litellm)). Its [benchmark page](https://docs.litellm.ai/docs/benchmarks) says "1,170 requests per second with 8 ms p95 gateway overhead across four instances". That overhead is what the proxy reports about itself, in its `x-litellm-overhead-duration-ms` response header, not a measurement from outside; the README calls it latency. The same run's request p95 is 150 ms. Conditions: 4 vCPUs and 8 GB per instance, a mock OpenAI endpoint, no version or date.
- Measured: LiteLLM 1.105.0's Python proxy, one worker per vCPU, added 107 ms at p50 and 185 ms at p99 ([results](#results)). This harness reports p50 and p99, not p95.
- Claim: "LiteLLM can be stress-tested at 1,000 QPS with no failures and can scale up to 5,000 QPS without failures on a 4-CPU, 8-GB RAM single instance setup." ([blog, 2026-02-02](https://docs.litellm.ai/blog/sub-millisecond-proxy-overhead)). Measured: under the stress scenarios it failed 104-132 requests and streams on every host ([failures](#failures)).
- LiteLLM's own [Rust gateway benchmark](https://docs.litellm.ai/blog/rust-ai-gateway-benchmarks) measures overhead as here, gateway minus direct, and puts its Python proxy at 257.7 ms p99; this harness measured 185 ms. That Rust gateway is a beta and is [not in these runs yet](roadmap.md#m5-benchmarks-and-optimization).

### agentgateway

- Its site and README make no numeric claim. Its [blog](https://agentgateway.dev/blog/2026-06-26-benchmarking-agentgateway-vs-litellm) measures p50 / p90 / p99 of 0.831 / 1.533 / 1.970 ms against LiteLLM's 7.076 / 17.986 / 32.192 ms, with 32 connections, 1 KB payloads, 3-second runs, and no hardware stated; scripts and data are public.
- Measured: agentgateway 1.7.0 added 0.91 ms at p50 and 1.7 ms at p99, LiteLLM 107 and 185 ms ([results](#results)). Its own figures for itself are close to these, and its direction, far ahead of LiteLLM, agrees.
- Solo.io's [product page](https://www.solo.io/products/agentgateway) claims 122× lower latency ("0.09ms vs 11ms") and 35× the throughput against a gateway it does not name, citing a repository whose author works at Solo.io. Nothing there can be checked, so I set no number beside it.

### KrakenD

- Claim: "Power your APIs and AI workloads with unrivaled speed. KrakenD handles thousands of API and AI requests per second, outperforming the competition." ([krakend.io](https://www.krakend.io)), with no conditions. Its [benchmark page](https://www.krakend.io/docs/benchmarks/) reports about 18,000 requests per second, measured with KrakenD 0.3.8 around 2016; it publishes no LLM numbers.
- Measured: KrakenD CE 3.0.1 added 461 µs at p50 against Wye's 325 µs, and leads bulk transfer ([where others lead](#where-others-lead)). CE buffers every response, so a stream's first byte arrives 44.5 ms late and paced events 209 ms late at p50 ([results](#results)).

## Where others lead

Each lead from the [raw results](../bench/reference/results/2026-11-05-7a83fae/), TLS + HTTP/2 unless noted:

- **KrakenD, bulk transfer.** 3.7 GB/s against Wye's 2.4 GB/s, at 0.96 ms of CPU per MB against 1.55 ms (plain HTTP: 0.85 against 1.81 ms). It reads the whole response, then writes it in a few large writes, faster than reading it direct (2.46 GB/s); Wye relays each frame as it arrives, which streaming requires. The same buffering gives it less CPU per event when many events arrive in one buffer, 0.55 against 1.17 µs for unpaced 64-byte events (plain: 0.47 against 5.3 µs). Uploads are a tie at 1.12 ms of CPU per MB each.
- **LiteLLM, memory per idle stream on plain HTTP.** 4.1 KB against Wye's 32 KB, on top of a process that holds 2.1 GB at idle (Wye: 28 MB). On TLS Wye's is lower, 17.5 against 45 KB.
- **agentgateway, close behind on stream latency.** Its delay per event is the nearest to Wye's, 109 against 77 µs p50 with 2,000 paced streams, 233 against 165 µs on unpaced streams. It leads on cold connections (32 first requests on a new process: 1.3 against 2.3 ms added p50) and on memory beside clients that stopped reading (22 against 537 KB per stalled stream). Wye's stalled stream holds about twice its 256 KiB HTTP/2 stream window ([why that size](research.md#performance-techniques)); a smaller window would raise bulk CPU per MB.

## Failures

A request or stream fails when it errors, ends early, or takes over 30 s ([what counts](development.md#benchmark)); each host's count is its worst of three repeats. The stress scenarios run 1,000 paced streams during back-to-back 1 MiB uploads, 8 at a time, then 1,000 more beside 200 clients that stopped reading.

- **Wye**: none, in any scenario of any [variant](#variants), on either path, on all three hosts.
- **LiteLLM**: 120, 132, and 104 failed requests and streams under the stress scenarios on the three hosts, and 1-2 failed ordinary requests per path on every host.
- **KrakenD**: one failed stream on each host (TLS path). Its idle, cancellation, and stress scenarios are not run. A buffering gateway cannot pass the first event of a stream that then goes silent.
- **Bifrost, agentgateway**: none.

## Current harness and baseline

`bench/baseline.json` holds Wye's metrics from the [results](#results) median, with the competitors' dropped. Every number in this section comes from it; the streams are complete Anthropic frames ([comparison runs](#comparison-runs)).

| Metric | Plain HTTP/1.1 | TLS + HTTP/2 |
|---|---|---|
| Added unary latency p50 / p99 | 254 / 559 µs | 325 / 628 µs |
| Direct unary latency p50, for scale | 109 µs | 180 µs |
| Added time to first byte p50 | 110 µs | 43 µs |
| Added delay per SSE event p50 / p99 | 156 / 349 µs | 165 / 237 µs |
| CPU per unary request | 42 µs | 62.5 µs |
| CPU per token-sized event (64 B, unpaced) | 5.3 µs | 1.17 µs |
| CPU per streamed MB (bulk) | 1.81 ms | 1.55 ms |
| Bulk throughput (direct 4.44 / 2.46 GB/s) | 2.17 GB/s | 2.44 GB/s |
| Memory per idle stream (RSS) | 32 KB | 17.5 KB |
| RSS at idle; after load | 28 MB; 43 MB | 28 MB; 194 MB |
| Upstream release after disconnect, p50 / max | 64 µs / 1.3 ms | 53 / 132 µs |
| 1 MiB upload: added p50, CPU per MB | 1.6 ms, 0.60 ms | 1.8 ms, 1.12 ms |
| Claude Code-shaped upload: added p50, CPU per MB | 1.6 ms, 0.60 ms | |
| 2,000 token-paced streams: event delay p50 / p99 | | 77 / 282 µs |
| 2,000 token-paced streams: CPU per event, memory per stream | | 14 µs, 12.6 KB |
| 2,000 token-paced streams: time to first byte p99 | | 742 µs |

Under stress (TLS path), 1,000 paced streams during 1 MiB uploads, 8 at a time, add 203 / 612 µs per event at p50 / p99 and 1.2 ms per upload at p50. Beside 200 stalled clients, events add 184 µs at p99, and each stalled stream holds 537 KB; those streams account for most of the gap between the two paths' memory after load, which is read after they close. The 32 first requests on a new process add 2.3 ms at p50 ([pre-warming](research.md#performance-techniques)).

Heap allocations (`alloc_`, plain path unless noted; spread across hosts under 1% per request, 2% per stream, 5% on upload counts, up to 13% on upload bytes):

| Metric | Value |
|---|---|
| Per streamed SSE event | 0.01 |
| Per stream setup | 22.7 |
| Per unary request | 20.6 (24.5 KB, with the JSON usage scanner) |
| Live heap per idle stream | 55 KB plain, 28 KB TLS |
| Live heap per idle stream after a 512 KB request | 97 KB plain, 53 KB TLS |
| Per 1 MiB upload | 23.1 allocations, 108 KB |
| Per Claude Code-shaped upload | 21.9 allocations, 88 KB |
| Live heap per stalled stream, TLS | 576 KB, about twice the 256 KiB upstream stream window |

### Variants

Each variant against plain Wye, plain path ([what each runs](development.md#benchmark)):

| Variant | Added p50 | CPU per request | Allocations per request | Claude Code-shaped upload, CPU per MB |
|---|---|---|---|---|
| Plain Wye | 254 µs | 42 µs | 20.6 | 0.60 ms |
| `auth_` | 256 µs | 42.5 µs | 20.5 | |
| `models_` | 273 µs | 44 µs | 21.6 | 1.19 ms |
| `diag_` | 257 µs | 43.5 µs | 22.6 | 1.49 ms |
| `otlp_` | 289 µs | 47 µs | | |
| `openai_` | 251 µs | 41.5 µs | | |
| `bedrock_` | 271 µs | 45 µs | 25.6 | |
| `failover_` | 267 µs | 45 µs | 27.6 | 0.60 ms |
| `refused_` | 302 µs | 51 µs | | 0.74 ms |

- **`auth_`**, Ed25519 gateway tokens: on a reused connection the per-connection cache makes verification free after the first request.
- **`models_`**, model permissions: one more allocation per request; the repeated-`model` check lexes the rest of the body, which doubles CPU per MB on Claude Code-shaped uploads and costs nothing on one-string bodies (1 MiB upload: 0.60 ms per MB).
- **`diag_`**, cache diagnostics: two more allocations per request and 2.5× the upload CPU on Claude Code-shaped bodies, hence per-session sampling.
- **`openai_`**, OpenAI Responses: 5.2 µs of CPU per event against 5.3 on Messages.
- **`bedrock_`**, a Claude route to Bedrock: five more allocations per request ([the hold](glossary.md#requests-and-where-they-go), the model's copy, the prefixed id, the new `content-length`, one unattributed).
- **`failover_`**, a route with failover: seven more allocations per request; a Claude Code-shaped upload makes 98.9 allocations and 1.13 MB instead of 21.9 and 88 KB, because kept chunks stop hyper from reusing its read buffer ([copying instead: rejected](research.md#performance-techniques)). CPU per MB is unchanged on this host.
- **`refused_`**, one upstream rejects one request in four: the rejected requests are resent; no failed requests.

Profiles of the plain, TLS, and paced paths, memory per stream by owner, and why Wye asks upstreams for uncompressed responses are in [research.md](research.md#performance-techniques).

For TLS uploads, Anthropic advertises 16 MiB frames but a 64 KiB stream window, which may limit an upload to window ÷ RTT (about 2 MB/s at 30 ms) unless Anthropic grows it; `scripts/upload-latency.sh` measures that against the real API (needs an API key). The default stays HTTP/2 until it does.

## Method

`wye-bench` runs each scenario directly against a mock Anthropic upstream (unary JSON, timestamped SSE) and through the gateway, a child process, and reports what the gateway adds. It runs on two upstream paths, plain HTTP/1.1 (low noise, best for catching regressions) and TLS + HTTP/2 (the production shape, `tls_` metrics), plus Wye variants and an allocation-counting build ([development.md](development.md#benchmark) lists them). Across the three hosts Wye's medians spread up to 12% on per-request latency and CPU, up to 18% on per-event delay and CPU, and more on paced tails and on time to first byte, whose TLS p50 is tens of µs.

Fair comparisons need identical hardware, payloads, keep-alive and TLS settings, upstream behavior, and features, and record hardware, build, workload, samples, percentiles, and errors. A gateway with more middleware enabled is not a fair baseline unless the difference is stated.

## Comparison runs

Competitors run with `--agentgateway`, `--bifrost`, `--litellm`, or `--krakend`, each at its documented best case for this traffic, with nothing turned on that Wye does not do:

- **Bifrost** v2.3.2 (Go), the official linux/amd64 binary. The request goes as Claude Code sends it (`claude-cli` user agent, a provider-qualified model), which Bifrost relays byte for byte instead of normalizing. No config or log store, request logging off, retries off; the pool and buffer sizes of Bifrost's own published benchmark (`initial_pool_size` 15,000, `buffer_size` 20,000) and 4,000 provider workers. Its upstream connection is HTTP/1.1 (fasthttp).
- **agentgateway** 1.7.0 (Rust, Linux Foundation), the GitHub release binary (fat LTO, jemalloc). Its LLM listener with one model in `detect` passthrough (the request relayed unchanged, its usage read, as Wye does), access log off, a 32 MiB request buffer. It offers HTTP/2 to TLS upstreams, as Wye does. Unlike Wye, it does not relay a unary response's bytes. It parses the JSON and writes it again, and answers `502 failed to process LLM response` to a reply without `model`.
- **LiteLLM** 1.105.0 on CPython 3.12, its Python proxy at its leanest, with byte-level passthrough (`pass_through_endpoints`; its own `/v1/messages` translates), no database, one worker per gateway vCPU.
- **KrakenD** CE 3.0.1 (Go) in passthrough (`no-op` encoding), access log off. CE cannot stream SSE. `no-op` buffers the whole response, and `sse` encoding is Enterprise-only.

Wye writes its usage records, as it does by default. Each gateway's generated configuration is in the [raw results](../bench/reference/results/2026-11-05-7a83fae/), next to `results.json`.

Gateways that parse requests and events need what real traffic always has, so comparison runs differ from gate runs in three ways, through every gateway alike:

- streams are complete Anthropic Messages streams (`message_start`, `text_delta` deltas carrying the send time in an extra field, `message_stop`); Bifrost drops events that are not Anthropic-shaped;
- the Claude Code-shaped upload says `"stream":false`, since the upload scenarios get a unary reply;
- a unary Messages reply carries `model`, `stop_reason`, and `stop_sequence`, as Anthropic's do; agentgateway refuses one without.

All runs send `"stream":true` on streaming requests and name the model `anthropic/bench`. Bifrost needs two more things. The mock's control headers go as `x-bf-eh-<name>`, the only extra headers it forwards, and requests leave `anthropic-version` to it, because its Anthropic route drops forwarded headers when the request carries one.

## Reference environment

Published numbers come from one pinned cloud machine type. `bench/reference/run.sh` builds a pushed commit on a fresh EC2 instance, runs the [comparison](#comparison-runs) against every competitor there, and copies the results into the repository. It is started only by hand; nothing in CI or on a schedule creates cloud resources, and the commit gate and the pull-request benchmark run on the committer's machine and GitHub's runners ([development.md](development.md#commit-gate)).

The machine and the split:

- One `c7i.4xlarge` in `us-east-1` (Intel Xeon Platinum 8488C, Sapphire Rapids; 8 cores, 16 vCPUs; 32 GiB), on demand, fixed performance rather than burstable, with nothing else on it. Ubuntu 24.04 LTS, the image Canonical's public SSM parameter names on the day, its id recorded with the results. `--instance-type` also takes `c7i.2xlarge` and `c7i.8xlarge`.
- The gateway runs on two physical cores with their hyperthreads (4 vCPUs, the last two cores, away from cpu0's interrupts), through `WYE_BENCH_GATEWAY_CPUS`; `wye-bench`, the load generator and the mock, runs on the other six (`taskset`). Every gateway gets the same four vCPUs, and LiteLLM one worker per vCPU. A larger instance only gives the load generator more room.
- One `wye-bench run --repeat 3` with Wye, its allocation-counting build, and all four competitors, so every gateway sees the same payloads on the same host in the same hour. Gateway output is discarded.

What the commit pins, since the instance clones it and runs its `bench/reference/remote.sh`:

| Component | Version | Artifact, checksum |
|---|---|---|
| Wye, `wye-bench` | the commit | built from source, `--locked`, the toolchain in `rust-toolchain.toml` |
| rustup | 1.29.3 | `static.rust-lang.org`, published SHA-256 |
| Bifrost | v2.3.2 | `downloads.getmaxim.ai` linux/amd64, SHA-256 of the first download (none published before v2.3.3) |
| agentgateway | 1.7.0 (`26f5aeb`) | GitHub release `agentgateway-linux-amd64`, published SHA-256 |
| KrakenD CE | 3.0.1 | GitHub release `generic-linux` tarball, its SHA-256 as GitHub lists it (KrakenD publishes SHA-512) |
| LiteLLM | 1.105.0, CPython 3.12 | `bench/reference/litellm-requirements.txt`: every dependency pinned by hash |
| AWS CLI | 2.37.12 | for the upload only; SHA-256 of the first download |

The Ubuntu image and the build packages from its mirrors (recorded per run), and the host EC2 places the instance on, are not pinned. `--runs N` repeats the run on N fresh instances, one after another, to measure that host-to-host variance.

Each run leaves `bench/reference/results/<date>-<commit>/run-<n>/`, committed with the numbers it backs, holding `results.json` and `table.txt` (Wye against each competitor), `system.json` (instance type, AMI, zone, kernel, CPU model, memory, `rustc`, Python, the CPU split, competitor versions), `lscpu.txt`, `run.json` (region, zone, AMI parameter, status, duration), each gateway's generated configuration, `remote.log`, and the Run Command output. `median.json` beside the runs merges them.

Unlike Bifrost's published method (t3 instances, one headline number, payloads that differ by instance size), it has a fixed-performance instance, every scenario with its percentiles and failure counts, the same payloads for every gateway on one host, and host variance measured rather than assumed.

To reproduce the [results](#results), in an account used for nothing else (the budget alarm covers the whole account):

```bash
export AWS_PROFILE=wye-refresh AWS_REGION=us-east-1
# Once per account: results bucket, the instance's role, a $20/month budget alarm.
aws cloudformation deploy --stack-name wye-bench-bootstrap \
  --template-file bench/reference/bootstrap.yaml --capabilities CAPABILITY_IAM \
  --parameter-overrides BudgetEmail=you@example.com
bench/reference/run.sh --commit 7a83fae --runs 3
dir=bench/reference/results/2026-11-05-7a83fae
target/release/wye-bench median "$dir/median.json" "$dir"/run-*/results.json
bench/reference/cleanup.sh        # deletes any leftover run stack
bench/reference/cleanup.sh --all  # also the bucket, role, and budget
```

`run.sh` deletes its stack on every exit, after a failure or Ctrl-C too, and waits until it is gone. If `run.sh` itself dies, the instance powers off 3 hours after boot and, by its shutdown behavior, terminates; the stack left behind costs nothing, and `cleanup.sh` removes it. The instance has no inbound rule. Run Command reaches it through the SSM agent, and its role may only write under `runs/` in the results bucket, whose objects expire after 30 days.

From the AWS price list (2026-11-05, `us-east-1`, on demand), `c7i.4xlarge` costs $0.714 per hour, its 40 GB gp3 volume about $0.004 per hour, its public IPv4 address $0.005 per hour. Each of the three runs took 28-31 minutes, about $0.37; all three about $1.10. The lifetime limit caps a forgotten run at about $2.20.

## Limits

What these numbers do not show:

- The upstream is a mock on the same host, with no network between gateway and provider and no real model's timing.
- One host, not two. The gateway and the load generator share the machine, separated by core pinning (two physical cores for the gateway, six for `wye-bench`) rather than a separate load machine. The gateway's 4 vCPUs are hyperthreads of 2 cores.
- Competitor configurations are each one's documented best case for this traffic, set by me; their generated files are in the raw results, so a better one can be proposed and rerun.
- LiteLLM is its Python proxy, not its Rust gateway, which is a beta.
- One commit (`7a83fae`) on one date, with each competitor's version as pinned that day.
- CPU time comes in 10 ms steps ([how it is read](development.md#benchmark)), so small per-MB figures such as 0.60 ms move a step at a time between hosts.
- No soak, capacity-per-core, or real-session figures yet. Those have not run in the reference environment.

## Benchmark suite: kept, cut, missing

The suite measures the gateway's own overhead under agent-shaped load, and each part has to justify its run time ([gated scenarios](development.md#benchmark)).

- Bulk throughput (MB/s) is reported only, being the least relevant to token-paced traffic; its CPU per MB stays gated.
- Competitors run only in comparison runs and the reference environment, never in the commit gate.
- Missing parts (agent trace replay, a floor baseline, a separate load machine) and the next competitors are planned in [roadmap.md](roadmap.md#m5-benchmarks-and-optimization).

I chose these competitors because KrakenD was the first reference (a mature general gateway), LiteLLM is the most-used open-source LLM gateway, Bifrost markets low overhead against LiteLLM (Wye's own claim), and agentgateway is the closest in kind.

## Gate tolerances

The commit gate runs on whatever machine commits, against the parent in alternating rounds ([development.md](development.md#commit-gate)). Until 2026-10-29 it compared each commit with a stored result instead. It then failed five times in a row on `sse_ttfb_added_p50_us` for code identical to a commit that had passed that morning. The binary that recorded the stored result measured about twice that value after the machine had been up 11 days, and something in between after a reboot. In the first run against the parent, an A/A comparison in effect, the gated metrics differed by a few percent at the median, and allocations by a few percent on uploads.

A metric passes when it stays under the parent's value times one plus its relative tolerance, plus an absolute floor. Eight A/A comparisons, on a quiet and on a busy machine, showed six floors too tight; each was raised to cover the worst excess seen plus about a quarter:

| Gate | Relative | Floor |
|---|---|---|
| `sse_ttfb_added_p50_us` | 40% | 300 µs |
| `auth_sse_ttfb_added_p50_us` | 40% | 350 µs |
| `openai_sse_ttfb_added_p50_us` | 40% | 350 µs |
| `sse_event_added_p50_us` | 20% | 160 µs |
| `tls_cold_unary_added_p50_us` | 150% | 7 ms |
| `tls_stalled_event_added_p99_us` | 50% | 3 ms |

Per-event overhead stays guarded by CPU per event and allocations per event. Under everyday load the new floors still failed now and then. On identical code a run and its rerun failed on different metrics (`alloc_upload_bytes_per_request`, then `tls_upload_added_p50_us`), and in both gate runs that failed twice on identical code the two comparisons shared no metric. Hence the gate's rule that only a metric failing both counts.
