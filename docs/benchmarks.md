# Benchmarks

Wye's [overhead](glossary.md#usage-and-cost), how it is measured, and how it compares. Unless noted, numbers come from one laptop (i9-9880H, 16 threads, macOS) over loopback against a mock upstream, release build, median of 3 runs. They are lab results, not production measurements.

## Method

`wye-bench` runs each scenario directly against a mock Anthropic upstream (unary JSON, timestamped SSE) and through a `wye` child process, and reports what Wye adds. Two upstream paths: plain HTTP/1.1 (low noise, best for catching regressions) and TLS + HTTP/2 (the production shape, `tls_` metrics), plus Wye variants and an allocation-counting build ([development.md](development.md#benchmark) lists them). `bench/baseline.json` records the numbers below; the [commit gate](development.md#commit-gate) does not read it, since it compares each commit with its parent ([its tolerances](#gate-tolerances)). Run-to-run noise: 10-15% on CPU per MB, ±25% on the TLS path, more on tails.

Fair comparisons need identical hardware, payloads, keep-alive and TLS settings, upstream behavior, and features, and record hardware, build, workload, samples, percentiles, and errors. A gateway with more middleware enabled is not a fair baseline unless the difference is stated.

## Current harness and baseline

From `bench/baseline.json` (recorded 2026-10-24 as a median of 7 on an idle machine, with the approved updates since; Intel i9-9880H, 8 cores, macOS):

| Metric | Plain HTTP/1.1 | TLS + HTTP/2 |
|---|---|---|
| Added unary latency p50 / p99 | 281 / 288 µs | 220 / 250 µs |
| Added time to first byte p50 | 205 µs | 178 µs |
| Added delay per SSE event p50 / p99 | 157 / 128 µs | 217 / 259 µs |
| CPU per unary request | 120.5 µs | 161 µs |
| CPU per token-sized event (64 B, unpaced) | 18.5 µs | |
| CPU per streamed MB (bulk) | 3.1 ms | 4.1 ms |
| Memory per idle stream (RSS) | 29 KB | 13 KB |
| Upstream release after disconnect, max | 119 µs | 113 µs |
| 1 MiB upload: added p50, CPU per MB | 3.1 ms, 1.30 ms | 2.9 ms, 2.61 ms |
| Claude Code-shaped upload: added p50, CPU per MB | 3.2 ms, 1.49 ms | |
| 2,000 token-paced streams: event delay p50 / p99 | | 1.4 / 4.0 ms |
| 2,000 token-paced streams: CPU per event, memory per stream | | 37 µs, 9.3 KB |
| Bulk throughput (direct 4.24 / 1.57 GB/s) | 1.30 GB/s | 1.07 GB/s |

Each added percentile is the gateway's minus the direct run's, so a p99 can read below its p50.

Heap allocations (`alloc_`, plain path; spread under 1%, uploads ~5%):

| Metric | Value |
|---|---|
| Per streamed SSE event | 0 |
| Per stream setup | 23 |
| Per unary request, plain and authenticated | 21.6 (24.5 KB, with the JSON usage scanner) |
| Live heap per idle stream | 51 KB plain, 24 KB TLS |
| Live heap per idle stream after a 512 KB request | 79 KB plain, 49 KB TLS (150 KB before the 32 KiB buffer cap) |
| Per 1 MiB upload | 45 allocations, 0.45 MB (1.27 MB before the cap) |
| Per Claude Code-shaped upload | 40.5 allocations, 0.38 MB |
| Live heap per stalled stream, TLS | 0.56 MB with a 256 KiB upstream window, 2.2 MB with 1 MiB |

A stalled stream's live heap is about twice the upstream window, independent of streams per connection.

Variants (plain path), each against plain Wye:

- **`auth_`**, Ed25519 gateway tokens: +88 µs CPU per full verification (the check itself 55-62 µs); on a reused connection the per-connection cache makes it free.
- **`models_`**, model permissions: +1 allocation per request; +0.67 ms/MB on Claude Code-shaped uploads (the repeated-`model` check lexes the rest of the body); free on one-string bodies.
- **`diag_`**, cache diagnostics: +2 allocations per request; +75% upload CPU on Claude Code-shaped bodies (2.6 vs 1.49 ms/MB), hence per-session sampling.
- **`otlp_`**, OTLP export: within noise (121.5 vs 120.5 µs CPU per request).
- **`openai_`**, OpenAI Responses: 25 µs CPU per event against 18.5 on Messages, because the mock's Responses events are ~50 bytes larger; with matched sizes (`WYE_BENCH_EVENT_BYTES=114` on Messages), 17.6-17.9 against 17.1-17.3 µs, so the Responses scanner adds about 0.5 µs per event in a real relay (caches, split lines), more than in its micro-benchmark below.
- **`bedrock_`**, a Claude route to Bedrock: +5 allocations per request ([the hold](glossary.md#requests-and-where-they-go), the model's copy, the prefixed id, the new `content-length`, one unattributed); CPU within noise (130.5 vs 120.5 µs, `models_` 124).
- **`failover_`**, a route with failover: +7 allocations per request; +25% CPU per MB on Claude Code-shaped uploads (1.86 vs 1.49 ms/MB) and 99.5 vs 40.5 allocations, because kept chunks stop hyper from reusing its read buffer ([copying instead: rejected](research.md#performance-techniques)).
- **`refused_`**, one upstream rejects one request in four: the rejected requests are resent, 149.5 µs CPU per request; no failed requests.

Measured outside the gate, against plain Wye:

- **Aliases**, every request aliased: one allocation per request for the hold, one more when the name's length changes, CPU per request within noise. Claude Code-shaped uploads 1.64 vs 1.49 ms per MB (within noise), since with aliases alone the rest of the body is not lexed; 2.16 with the repeated-`model` check.
- **Upgrade checks**: on a Claude Code-shaped 1 MiB body the scan takes 0.77× the time of the model-permission scan and half that of cache diagnostics (same machine, same body), about 0.5 ms per MB in the gateway.
- **Usage scanners**, a release micro-benchmark (200,000 token-sized events fed one per call, best of 7, 2026-10-26): 27 ns per event on Messages (124 B), 34 on Responses (173 B), 40 on Chat Completions (180 B), whose path the gate's mock does not serve. Lexing a 1.2 MiB Responses terminal event, or a JSON body of that size, takes 0.56-0.61 ms per MiB at 16 or 64 KiB chunks.
- **Agent attribution**, measured when it was built: capturing Claude Code's agent headers costs 0.04-0.32 µs per request, validating them 0.11 µs in the writer task, and reading tool names from `content_block_start` 0.22-0.33 µs per block, with no allocation.

**Stress** (TLS path, median of 7, 2026-10-24). 1,000 paced streams while 1 MiB uploads run 8 at a time: events +1.0 / 1.9 ms p50 / p99, uploads +2.4 ms p50. Beside 200 clients that stopped reading: events +2.7 ms p99, no failures (memory per stalled stream: the allocation table above); RSS stays at its peak after they close, because the allocator keeps the freed pages. 32 first requests on a new process: +2.9 ms p50 ([pre-warming](research.md#performance-techniques)).

**Soak** (`wye-bench load --scenario paced --seconds 150`, alloc-stats build): 34 batches of 2,000 paced streams, 6.8M events, zero failures, 34 µs CPU per event. Live heap rose ~600 KB over the first ~20 batches (lanes and buffers warming), then held at 49,363 KB; RSS levelled near 56 MB. No growth per stream served. A multi-hour soak is open: the overnight attempt (2026-10-18) was void because the laptop slept.

**Capacity per core** (`--scenario capacity`, one Tokio worker, TLS + HTTP/2, 50 events/s of 100 B per stream, 2 runs):

| Streams | Events/s | Wye cores | CPU µs/event | Direct p99 | Added p99 |
|---|---|---|---|---|---|
| 500 | 11,900 | 0.24 | 20 | 0.9-1.1 ms | 4.1-4.4 ms |
| 1,000 | 23,900 | 0.37-0.39 | 16 | 1.5-2.2 ms | 6.0-6.4 ms |
| 1,500 | 35,800-35,900 | 0.46-0.48 | 13 | 2.6-3.0 ms | 8.3-8.9 ms |
| 2,000 | 47,900 | 0.57-0.58 | 12 | 3.2-3.3 ms | 12.1-13.0 ms |
| 3,000 | 70,500-71,300 | 0.71-0.73 | 10 | 4.1-4.3 ms | 67-79 ms |
| 4,000 | 88,400-91,000 | 0.74-0.76 | 8 | 5.2-5.3 ms | 219-258 ms |

Sustainable (≤10 ms added p99): 1,500 streams, ~36,000 events/s per worker. The limit is queueing on the thread, not CPU. Two workers held 3,000 streams (linear); four were limited by the laptop's load generator (4-5 cores at 3,000 streams), so multi-core capacity needs a separate load machine. CPU per event rises with worker count (12-20 µs at 1, 14-27 at 2, 21-38 at 4): cross-thread wakeups. A smaller default worker count is worth measuring on a real host.

Profiles of the plain, TLS, and paced paths, memory per stream by owner, and the measurement behind asking upstreams for uncompressed responses: [research.md](research.md#performance-techniques).

**TLS uploads** (open gap: KrakenD 2.0 vs Wye 2.6-3.1 ms CPU per MB). Like for like over HTTP/1.1, Wye is ~10% behind; HTTP/2 adds ~9% more. Of HTTP/2 busy samples: 45% rustls `writev`, 16% AES-GCM, 15% `recvfrom`, 10% the h2 mutex; h2 flushes one 16 KiB frame at a time (64 KiB frames: 2.59 ms; 256 KiB: 2.29 ms). Anthropic advertises 16 MiB frames but a 64 KiB stream window, which may limit an upload to window ÷ RTT (~2 MB/s at 30 ms) unless Anthropic grows it; `scripts/upload-latency.sh` measures that against the real API (needs an API key). The default stays HTTP/2 until it does.

## In use

The gateway this laptop runs for its own Claude Code sessions, measured 2026-11-04: `wye 0.1.0-rc.1` built 2026-11-03, the passthrough setup, api.anthropic.com over TLS + HTTP/2 on Wi-Fi. CPU from `ps`, bytes from the upstream connections (`nettop`), requests from the usage log.

| Measure | Value |
|---|---|
| Uptime; requests; output tokens | 35 h; 2,562; 2.3 million |
| CPU, all of it | 31.4 s (12 ms per request) |
| Memory (RSS; physical footprint) | 24.8 MB; 18.5 MB |
| CPU while no request is open | none measurable |
| 8 busy minutes: requests, bytes sent upstream, bytes back | 28; 100 MB; 0.8 MB |
| 8 busy minutes: CPU | 0.61 s, 6.1 ms per MB sent |
| Busiest 30 s | 3.3 ms of CPU per second |

Claude Code resends its context with every request, 3.6 MB per request in those minutes, so uploads, not streamed events, set the cost. The lab's unit costs predict 0.38 s of the 0.61: about 2 ms per MB on this path (a plain client and a TLS upstream, between the two upload figures above) and 37 µs for each of about 5,000 paced events. The other 2.3 ms per MB is sending over a real network rather than loopback: curl, uploading 32 MB over TLS from the same laptop, used 4.1 ms of CPU per MB. A client calling the provider directly pays that itself.

The busiest 30 s used a third of the [sizing estimate](operations.md#sizing-estimate) for a heavy user (10 ms per second), and the memory is within the 15-27 MB at idle of the comparison runs below.

## Against KrakenD

KrakenD 3.0.1 CE in pass-through (`no-op` encoding) at its best case (no access log); Wye keeps its usage log. Single run, all 16 threads, 2026-10-15.

KrakenD CE cannot stream SSE: `no-op` buffers the whole response (an event sent at 0 s arrived at 3 s); `sse` encoding is Enterprise-only.

| Metric | Wye | KrakenD |
|---|---|---|
| Added unary latency p50 / p99, TLS | 260 / 629 µs | 371 / 2,301 µs |
| CPU per request, TLS | 161 µs | 252 µs |
| Added time to first byte p50, TLS | 0.45 ms | 72 ms (buffered) |
| 2,000 paced streams: event delay p50 / p99 | 1.5 / 5.4 ms | 295 / 572 ms (buffered) |
| 2,000 paced streams: CPU per event, memory per stream | 35 µs, 7.9 KB | 55 µs, 95 KB |
| RSS after load, TLS | 53 MB | 302 MB |
| 1 MiB upload, TLS: added p50, CPU per MB | 3.7 ms, 3.1 ms | 1.9 ms, 2.0 ms |
| CPU per streamed MB (bulk), TLS | 4.4 ms | 1.5 ms |

KrakenD leads only bulk transfer and TLS uploads: it writes buffered responses in a few large writes; Wye relays each frame as it arrives, which streaming requires, and crosses a task boundary per chunk. Adaptive write coalescing gained nothing (hyper already batches ready frames into one `writev`).

## Against LiteLLM

LiteLLM 1.105 (Python 3.14) at its leanest: byte-level pass-through (`pass_through_endpoints`; its own `/v1/messages` translates), no database, one worker per core; CPU and memory summed over the process tree. Single run, 2026-10-15, zero failures on both.

| Metric (TLS unless noted) | Wye | LiteLLM |
|---|---|---|
| Added unary latency p50 / p99 | 0.26 / 0.63 ms | 44 / 141 ms |
| CPU per request | 161 µs | 19.8 ms |
| Added time to first byte p50 | 0.45 ms | 14 ms |
| 2,000 paced streams: event delay p50 / p99 | 1.5 / 5.4 ms | 34 ms / 0.61 s |
| 2,000 paced streams: CPU per event | 35 µs | 312 µs |
| 1 MiB upload: added p50, CPU per MB | 3.7 ms, 3.1 ms | 54 ms, 52 ms |
| Claude Code-shaped upload, plain: CPU per MB | 1.49 ms | 190 ms |
| Memory per idle stream; process at idle | 6.8 KB; 27 MB | 41 KB; 8.7 GB |

Caveats: 16 LiteLLM workers compete with the load generator on one laptop; its regular API path would add more overhead than pass-through. The comparison found four flaws in the harness, all fixed: invalid JSON in mock payloads, error responses counted as successes (now `*_failed_requests`, gated at zero), a test CA Python 3.13 rejects, and gateway workers outliving their run.

## Against Bifrost

Bifrost v2.3.2 (Go), the official prebuilt binary (`downloads.getmaxim.ai`, darwin/amd64), at its best case: the request as Claude Code sends it (`claude-cli` user agent, a provider-qualified model), which Bifrost relays byte for byte instead of normalizing; no config or log store, request logging off, retries off; the pool and queue sizes of Bifrost's own published benchmark (`initial_pool_size` 15,000, `buffer_size` 20,000) and 4,000 provider workers, above this benchmark's peak. Its upstream connection is HTTP/1.1 (fasthttp). Median of 3, 2026-10-26, zero failures on both. Streams are complete Anthropic frames for both gateways ([below](#comparison-runs)).

| Metric (TLS unless noted) | Wye | Bifrost |
|---|---|---|
| Added unary latency p50 | 0.25 ms | 2.7 ms |
| CPU per request | 157 µs | 1,044 µs |
| Added time to first byte p50 | 0.39 ms | 4.5 ms |
| Added delay per SSE event p50 / p99 | 195 / 219 µs | 190 / 671 µs |
| 2,000 paced streams: event delay p50 / p99 | 2.1 / 4.3 ms | 1.6 / 58 ms |
| 2,000 paced streams: time to first byte p99 | 9 ms | 0.99 s |
| 2,000 paced streams: CPU per event | 37 µs | 162 µs |
| Beside 200 stalled clients: event p99 | 3.0 ms | 229 ms |
| Paced streams during uploads: event p99 | 2.3 ms | 30 ms |
| 1 MiB upload: CPU per MB | 2.6 ms | 45 ms |
| Claude Code-shaped upload, plain: CPU per MB | 1.56 ms | 74 ms |
| Bulk relay: CPU per MB | 4.3 ms | 25 ms |
| Upstream release after a client disconnect, p50 | 0.08 ms | 498 ms |
| 32 first requests on a new process: added p50 | 3.2 ms | 84 ms |
| Process at idle; after load (default pools) | 15 MB; 179 MB | 328 MB; 3.7 GB |

Where Bifrost leads, and why:
- Paced event p50 (1.6 vs 2.1 ms, this run only): Bifrost queues stream starts (time to first byte p99 0.99 s), so fewer of the 2,000 streams overlap and those running see less contention; its p99 is 58 ms. In the other two runs its paced p50 was 129 and 159 ms.
- Per-event delay p50 is a tie within noise: Wye's swung from 195 to 375 µs between identical runs, Bifrost's from 190 to 272 µs. Bifrost's upstream is HTTP/1.1 and the direct baseline is HTTP/2, so an HTTP/1.1 relay also skips the mock's HTTP/2 framing. With Wye on HTTP/1.1 as well (`http_version = "1.1"`): 217 vs 243 µs p50, 236 vs 1,502 µs p99. Wye keeps HTTP/2 as the default because one connection per stream adds more latency under load (paced event p50 3.4 ms on HTTP/1.1).
- Memory per paced stream reads lower (5 vs 9.6 KB), but from an idle process 20 times larger.

Load generator, mock, and gateway share one laptop (8 physical cores); at 2,000 paced streams Wye alone uses ~3.7 cores and Bifrost all 16 threads, so paced latencies include machine contention. Bifrost's published "11 µs" counts only its internal stages ("excludes JSON marshalling and HTTP calls"); the numbers here are end to end, what a client sees.

## Against agentgateway

[agentgateway](https://github.com/agentgateway/agentgateway) 1.7.0 (Rust, Linux Foundation), the closest in kind: a proxy for AI agents and model providers. Built from source (`26f5aeb`) with its own release profile (fat LTO, one codegen unit) and default features (jemalloc), since there is no Intel macOS binary; native, not in a container. Its LLM listener at its best case for this traffic: one model in `detect` passthrough (the request relayed unchanged, its usage read, as Wye does), access log off, a 32 MiB request buffer. It offers HTTP/2 to TLS upstreams, as Wye does. Median of 3, 2026-11-01, zero failures on both.

| Metric (TLS unless noted) | Wye | agentgateway |
|---|---|---|
| Added unary latency p50 / p99 | 0.26 / 0.36 ms | 1.8 / 2.7 ms |
| Added unary latency p50, plain | 0.27 ms | 0.42 ms |
| CPU per request | 171 µs | 536 µs |
| Added time to first byte p50 | 0.24 ms | 0.63 ms |
| Added delay per SSE event p50 / p99 | 233 / 244 µs | 408 / 635 µs |
| 2,000 paced streams: event delay p50 / p99 | 1.2 / 3.1 ms | 1.0 / 3.6 ms |
| 2,000 paced streams: time to first byte p99 | 5.1 ms | 4.4 ms |
| 2,000 paced streams: CPU per event, memory per stream | 37 µs, 10 KB | 42 µs, 37 KB |
| Paced streams during uploads: event p99; upload p50 | 1.8 ms; 2.6 ms | 1.0 ms; 11 ms |
| Beside 200 stalled clients: event p99; memory per stalled stream | 2.9 ms; 472 KB | 2.2 ms; 145 KB |
| 1 MiB upload: added p50, CPU per MB | 2.7 ms, 2.6 ms | 16 ms, 4.0 ms |
| Claude Code-shaped upload, plain: CPU per MB | 1.56 ms | 24 ms |
| Bulk relay: CPU per MB; throughput (direct 1.6 GB/s) | 4.8 ms; 0.98 GB/s | 13 ms; 0.31 GB/s |
| 32 first requests on a new process: added p50 | 3.2 ms | 2.4 ms |
| Memory per idle stream; process after load | 13 KB; 191 MB | 47 KB; 562 MB |

Where agentgateway leads, and why:
- Paced and mixed-load event latency (p50 1.0 vs 1.2 ms; event p99 during uploads 1.0 vs 1.8 ms); an earlier single run agreed. Its uploads take 4× longer there (11 vs 2.6 ms p50), so less upload traffic overlaps the streams.
- Memory beside stalled clients: 145 vs 472 KB per stalled stream. Wye's bound is twice its 256 KiB HTTP/2 stream window ([why that size](research.md#performance-techniques)); a smaller window would raise bulk CPU per MB. Worth an A/B before 1.0.
- Cold connections (2.4 vs 3.2 ms p50), a metric that swings 3-9 ms with load.

Unlike Wye, it does not relay a unary response's bytes: in passthrough it parses the JSON and writes it again, with its fields in another order, and answers `502 failed to process LLM response` to a reply without `model`.

## Comparison runs

Competitors run with `--agentgateway`, `--bifrost`, `--litellm`, or `--krakend`. Gateways that parse requests and events need what real traffic always has, so comparison runs differ from gate runs in three ways, through every gateway alike:
- streams are complete Anthropic Messages streams (`message_start`, `text_delta` deltas carrying the send time in an extra field, `message_stop`); Bifrost drops events that are not Anthropic-shaped;
- the Claude Code-shaped upload says `"stream":false`, since the upload scenarios get a unary reply;
- a unary Messages reply carries `model`, `stop_reason`, and `stop_sequence`, as Anthropic's do; agentgateway refuses one without.

All runs send `"stream":true` on streaming requests and name the model `anthropic/bench`. Bifrost needs two more things: the mock's control headers go as `x-bf-eh-<name>`, the only extra headers it forwards, and requests leave `anthropic-version` to it, because its Anthropic route drops forwarded headers when the request carries one.

## Benchmark suite: kept, cut, missing

The suite measures the gateway's own overhead under agent-shaped load, and each part has to justify its run time ([gated scenarios](development.md#benchmark)).

- Bulk throughput (MB/s) is reported only, being the least relevant to token-paced traffic; its CPU per MB stays gated.
- KrakenD is retired from routine runs: it cannot stream SSE on CE, and the result [above](#against-krakend) stands.
- Missing parts (agent trace replay, a floor baseline, a separate Linux load machine) and the next competitors are planned in [roadmap.md](roadmap.md#m5-benchmarks-and-optimization).

Why these competitors: KrakenD was the first reference (a mature general gateway), LiteLLM is the most-used open-source LLM gateway, Bifrost markets low overhead against LiteLLM (Wye's own claim), and agentgateway is the closest in kind. Each runs in pass-through at its best case, with its configuration recorded here.

## Gate tolerances

The evidence behind the [commit gate](development.md#commit-gate)'s method and tolerances.

Until 2026-10-29 the gate compared each commit with a stored result. It then failed five times in a row on `sse_ttfb_added_p50_us` for code identical to a commit that had passed that morning: the binary that recorded the 205 µs stored baseline measured 406 µs on the same laptop after 11 days up, and 264-317 µs after a reboot. In the first run against the parent in alternating rounds, an A/A in effect, the median difference across 69 gated metrics was 3.8%, and allocations differed by at most 7% (uploads).

Eight A/A comparisons, quiet and busy (load average 6-10), showed six absolute floors too tight; each now covers the worst excess seen plus about 25%:

| Gate | Worst excess | Floor |
|---|---|---|
| `sse_ttfb_added_p50_us` | 231 µs (busy) | 300 µs |
| `auth_sse_ttfb_added_p50_us` | 277 µs (busy) | 350 µs |
| `openai_sse_ttfb_added_p50_us` | 260 µs (busy; 143 quiet) | 350 µs |
| `sse_event_added_p50_us` | 122 µs (busy; under 20 quiet) | 160 µs |
| `tls_cold_unary_added_p50_us` | 5.3 ms (busy) | 7 ms |
| `tls_stalled_event_added_p99_us` | 2.0 ms (quiet) | 3 ms |

Per-event overhead stays guarded by CPU per event and zero allocations per event. Under everyday load the new floors still failed now and then: on identical code a run and its rerun failed on different metrics (`alloc_upload_bytes_per_request`, then `tls_upload_added_p50_us`), and in both gate runs that failed twice on identical code the two comparisons shared no metric. Hence the gate's rule that only a metric failing both counts.
