# Research

Measured results, open questions, and directions; nothing here is a product promise. Unless noted, numbers come from one laptop (i9-9880H, 16 threads, macOS) over loopback against a mock upstream, release build, median of 3 runs. They are lab results, not production measurements.

## Questions to test

- What TTFT and inter-event latency does Wye add over direct access, under load and with slow readers?
- How fast does a disconnect release upstream work, and does the provider stop generation or billing?
- How much does cache-affine selection raise cache hit rate and lower billed cost on real agent workloads?
- How loose is the provable cost bound (bound ÷ actual cost) on real sessions?
- Which results hold only against the mock?

A proxy adds a hop and cannot beat direct access on latency. Local cancellation does not prove the provider stopped billing. Real-provider experiments are opt-in and record model, usage, cache behavior, pricing, and billed cost.

## Verification needed

Provider facts a feature depends on; each is confirmed, with a source, before that feature ships.

| Assumption | Depends on it | How to check |
|---|---|---|
| Input tokens ≤ UTF-8 bytes per tokenizer, including template tokens | [Provable cost ceiling](policy.md#provable-cost-ceiling) | Tokenizer construction; adversarial requests against reported usage |
| Output-token limits cap reasoning tokens, per model | Provable cost ceiling | Low limits with reasoning on; compare usage |
| Anthropic per-image token maximum; tool-use system-prompt overhead; prompt-length price tiers | Provable cost ceiling | Provider docs; requests with and without images and `tools` |
| Bound ÷ actual cost on real Claude Code sessions | Positioning the ceiling as a guard | Opt-in telemetry |
| Which providers enforce workspace spend limits as hard stops, with what lag | [Provider-held budgets](policy.md#provider-held-budgets) | A low-limit project driven past its limit |
| Claude Code's gateway contract per release | M0 | [Gateway guide](https://code.claude.com/docs/en/llm-gateway-protocol); contract tests per release |
| A process `ANTHROPIC_BASE_URL` overrides one in Claude Code settings | `wye claude` | Launch with both set |
| Share of `auxiliary`, `compaction`, `subagent` requests, and quality when each goes to a smaller model | [Agent-aware routing](#1-agent-aware-routing) | Opt-in telemetry with hint headers; task-level evaluation |
| Codex's transport and base-URL support for Responses | M3 | Read from Codex's source (970111f) and gateway guide, 2026-10-26 ([architecture.md](architecture.md#codex-integration)); one real Codex session through Wye still open |
| OpenAI `input_tokens` include `cached_tokens`; reasoning tokens are part of `output_tokens` | Responses cost | OpenAI API reference; one recorded response with a cache hit |
| Which SDKs tolerate the extra empty-`choices` usage chunk | `include_usage` default | SDK conformance tests |
| Bedrock's Messages endpoint takes its API key in `x-api-key` (Anthropic's page) or `Authorization: Bearer` (AWS's page), and its `anthropic.`-prefixed ids for every current model | Bedrock provider (built with `x-api-key`, M4 stage 12) | One request with each header and a recorded stream |
| Bedrock's Messages endpoint and Claude Platform on AWS accept SigV4 with `UNSIGNED-PAYLOAD` | Signing streamed bodies ([M4](architecture.md#claude-across-providers-m4)) | One signed request per endpoint |
| Vertex's streaming URL and framing; `count_tokens` and `anthropic-beta` on each Claude provider | M4 | Platform docs; one recorded stream per provider |
| `429`/`529` from each Claude provider means the request was not processed | Failover without duplicate billing | Provider docs; billing records |
| Each upgrade-check rule (a field a model rejects) | [Upgrade checks](architecture.md#model-aliases-and-upgrade-checks) | Built from Anthropic's migration guide (2026-10-26); one opt-in live request per rule still open |

## Benchmarks

**Method.** `wye-bench` runs each scenario directly against a mock Anthropic upstream (unary JSON, timestamped SSE) and through a `wye` child process, and reports what Wye adds. Two upstream paths: plain HTTP/1.1 (low noise, best for catching regressions) and TLS + HTTP/2 (the production shape, `tls_` metrics), plus Wye variants and an allocation-counting build ([development.md](development.md#benchmark) lists them). The commit gate runs every commit and its parent in alternating rounds and compares the medians ([development.md](development.md#commit-gate)); `bench/baseline.json` records the numbers below. Run-to-run noise: 10-15% on CPU per MB, ±25% on the TLS path, more on tails; against a stored baseline the gate caught ~25% or ~100 µs ([under alternating rounds](#gate-method-the-parent-in-alternating-rounds)).

Fair comparisons need identical hardware, payloads, keep-alive and TLS settings, upstream behavior, and features, and record hardware, build, workload, samples, percentiles, and errors. A gateway with more middleware enabled is not a fair baseline unless the difference is stated.

### Current harness and baseline

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
| Live heap per stalled stream, TLS | 0.56 MB with a 256 KiB upstream window, 2.2 MB with 1 MiB: about twice the window, independent of streams per connection |

Variants (plain path):

| Variant | Cost against plain Wye |
|---|---|
| `auth_`: Ed25519 tokens | +88 µs CPU per full verification (the check itself 55-62 µs); on a reused connection the per-connection cache makes it free |
| `models_`: model permissions | +1 allocation per request; +0.67 ms/MB on Claude Code-shaped uploads (the repeated-`model` check lexes the rest of the body); free on one-string bodies |
| `diag_`: cache diagnostics | +2 allocations per request; +75% upload CPU on Claude Code-shaped bodies (2.6 vs 1.49 ms/MB), hence per-session sampling |
| `otlp_`: OTLP export | within noise (121.5 vs 120.5 µs CPU per request) |
| `openai_`: OpenAI Responses | 25 µs CPU per event against 18.5 on Messages, from larger events ([below](#openai-stream-scanners-micro-benchmark)) |
| `bedrock_`: a Claude route to Bedrock | +5 allocations per request (the hold, the model's copy, the prefixed id, the new `content-length`, one unattributed); CPU within noise (130.5 vs 120.5 µs, `models_` 124) |
| `failover_`: a route with failover | +7 allocations per request; +25% CPU per MB on Claude Code-shaped uploads (1.86 vs 1.49 ms/MB) and 99.5 vs 40.5 allocations, because kept chunks stop hyper from reusing its read buffer ([copying instead: rejected](#performance-techniques)) |
| `refused_`: one account refuses one request in four | refused requests resent: 149.5 µs CPU per request; no failed requests |

**Stress** (TLS path, median of 7, 2026-10-24). 1,000 paced streams while 1 MiB uploads run 8 at a time: events +1.0 / 1.9 ms p50 / p99, uploads +2.4 ms p50. Beside 200 clients that stopped reading: events +2.7 ms p99, 0.49 MB RSS per stalled stream with the 256 KiB upstream window (2.0 MB with 1 MiB), no failures; RSS stays at its peak after they close, because the allocator keeps the freed pages. 32 first requests on a new process: +2.9 ms p50 ([pre-warming](#performance-techniques)).

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

**Profiles.** Plain path: almost all system calls (`writev`, `recvfrom`). TLS path: first dominated by h2's per-connection mutex; spreading streams over 64 connections ([architecture.md](architecture.md#upstream-connections)) cut CPU per MB 12.3 → 4.1 ms and unary added latency 1.5 → 0.3 ms. What remains: relaying each frame as it arrives, and handing each chunk between the client and upstream connection tasks. Paced load, TLS, 2026-10-24 (`sample`, 10 s of 2,000 streams): of busy samples, `writev` to clients 43% (one write per event, inherent at token pace), `recvfrom` from the upstream 16%, waits on h2's per-connection lock 10% (`poll_data`, `is_end_stream`), Wye's own code under 1%. Memory per stream, 2026-10-25 (`heap` and `malloc_history`, 2,000 paced TLS streams, `MallocStackLogging=1`): of the ~24 KB of heap per stream, hyper's HTTP/1.1 client connection holds a fixed 8.5 KB read buffer and an 8.5 KB write buffer (70%), its dispatcher 3.5 KB, the Tokio task 0.9 KB, request headers 0.7 KB, timer and service closure 0.2 KB; Wye's own state is not among the top sites. hyper 1.x exposes only a cap on those buffers (`max_buf_size`, set to 32 KiB), not their initial size, so shrinking them needs a hyper change.

**Compressed responses** (2026-10-17, first real session): every stream was recorded `usage_status: unknown`. Claude Code asks for `gzip, deflate, br, zstd`; Anthropic answers Brotli; the mock never compresses, so no test caught it. Fix: ask the upstream for `identity`. Its cost, from 353 real responses rebuilt as streams and compressed with a flush per event:

| Delta size | Identity per response | Per event | gzip -6 | Brotli q4 / q11 | zstd -3 |
|---|---|---|---|---|---|
| 8 B | 35.9 KB | 151 B | 4.0× | 5.7× / 5.8× | 5.8× |
| 16 B | 21.5 KB | 173 B | 3.5× | 4.5× / 4.6× | 4.6× |
| 32 B | 14.3 KB | 211 B | 3.1× | 3.6× / 3.7× | 3.7× |

Identity costs 10-30 KB more per response, ~28 kbit/s while streaming: 0.12 ms per event on a 10 Mbit/s link against a 20 ms token gap. Decompressing a copy instead costs 6-11 µs per event (20-90% of Wye's relay cost), needs an attacker-sized decoder window per stream, and ends zero allocations per event. Rejected.

**TLS uploads** (open gap: KrakenD 2.0 vs Wye 2.6-3.1 ms CPU per MB). Like for like over HTTP/1.1, Wye is ~10% behind; HTTP/2 adds ~9% more. Of HTTP/2 busy samples: 45% rustls `writev`, 16% AES-GCM, 15% `recvfrom`, 10% the h2 mutex; h2 flushes one 16 KiB frame at a time (64 KiB frames: 2.59 ms; 256 KiB: 2.29 ms). Anthropic advertises 16 MiB frames but a 64 KiB stream window, which may limit an upload to window ÷ RTT (~2 MB/s at 30 ms) unless the server grows it; `scripts/upload-latency.sh` measures that against the real API (needs a key). The default stays HTTP/2 until it does.

### Against KrakenD

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

### Against LiteLLM

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

Caveats: 16 LiteLLM workers compete with the load generator on one laptop; its regular API path would cost more than pass-through. The comparison found four flaws in the harness, all fixed: invalid JSON in mock payloads, error responses counted as successes (now `*_failed_requests`, gated at zero), a test CA Python 3.13 rejects, and gateway workers outliving their run.

### Against Bifrost

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
- Per-event delay p50 is a tie within noise: Wye's swung from 195 to 375 µs between identical runs, Bifrost's from 190 to 272 µs. Bifrost's upstream is HTTP/1.1 and the direct baseline is HTTP/2, so an HTTP/1.1 relay also skips the mock's HTTP/2 framing. With Wye on HTTP/1.1 as well (`http_version = "1.1"`): 217 vs 243 µs p50, 236 vs 1,502 µs p99. Wye keeps HTTP/2 as the default because one connection per stream costs more under load (paced event p50 3.4 ms on HTTP/1.1).
- Memory per paced stream reads lower (5 vs 9.6 KB), but from an idle process 20 times larger.

Load generator, mock, and gateway share one laptop (8 physical cores); at 2,000 paced streams Wye alone uses ~3.7 cores and Bifrost all 16 threads, so paced latencies include machine contention. Bifrost's published "11 µs" counts only its internal stages ("excludes JSON marshalling and HTTP calls"); the numbers here are end to end, what a client sees.

#### Comparison runs

Competitors run with `--bifrost`, `--litellm`, or `--krakend`. Gateways that parse requests and events need what real traffic always has, so comparison runs differ from gate runs in two ways, through every gateway alike:
- streams are complete Anthropic Messages streams (`message_start`, `text_delta` deltas carrying the send time in an extra field, `message_stop`); Bifrost drops events that are not Anthropic-shaped;
- the Claude Code-shaped upload says `"stream":false`, since the upload scenarios get a unary reply.

All runs send `"stream":true` on streaming requests and name the model `anthropic/bench`. Bifrost needs two more things: the mock's control headers go as `x-bf-eh-<name>`, the only extra headers it forwards, and requests leave `anthropic-version` to it, because its Anthropic route drops forwarded headers when the request carries one.

### Benchmark suite: kept, cut, missing

The suite measures the gateway's own cost under agent-shaped load, and each part has to justify its run time. Approved 2026-10-24; the stress scenarios are built, and of the proposed competitors Bifrost has run (M5).

| Part | Verdict | Why |
|---|---|---|
| TLS + HTTP/2 path, paced streams, uploads, idle memory, cancel, allocations | Gated | The production shape and the costs agents feel |
| Plain HTTP/1.1 path | Gated | Lowest noise: catches regressions the TLS path hides |
| `auth_`, `models_`, `diag_` variants | Gated | Each guards a per-request or per-MB cost of a feature |
| `otlp_` variant | Gated | Export is on in every team deployment; reported-only metrics regress silently |
| Bulk throughput (MB/s) | Reported; CPU per MB gated | Least relevant to token-paced traffic; CPU per MB stays gated |
| KrakenD runs | Retired from routine runs | Cannot stream SSE on CE; the recorded result above stands |
| Mixed load (`mixed_`) | Built | Uploads and paced streams at once: head-of-line effects on event p99 when agents send MB contexts |
| Cold connections (`cold_`) | Built | First requests on a new process: connects and TLS handshakes |
| Stalled clients (`stalled_`) | Built | Memory and event delay beside readers that stopped; found the shared-window stall on its first run |
| Provider variants (`openai_`, `bedrock_`, `failover_`) | Gated | Each guards a hot path a feature adds: another API's scanners, the provider's model id, the kept body for resending |
| Refusals (`refused_`) | Gated | One account refuses one request in four before reading it; found uploads cut off by an early refusal ending in a 502 on its first run (4.8% of 1 MiB uploads), fixed |
| Missing: agent trace replay | Add (M5) | [TraceLab](#related-work) context sizes, output lengths, turn gaps |
| Missing: floor baseline | Add (M5) | Wye against a dumb streaming proxy shows what its features cost |
| Missing: separate load machine, Linux | Add (M5) | The laptop's load generator saturates first at multi-core load |

**Competitors.** KrakenD was the first reference (a mature general gateway); LiteLLM is the most-used open-source LLM gateway. Neither is Wye's closest rival now. Proposed set, each in pass-through at its best case:

| Gateway | Why |
|---|---|
| nginx or Envoy, plain streaming proxy (`proxy_buffering off`) | The floor: the cost of a proxy that does nothing |
| Envoy AI Gateway | The infrastructure-standard data plane with LLM routing |
| Bifrost (Go) | Done ([above](#against-bifrost)): markets low overhead against LiteLLM, Wye's own claim |
| LiteLLM Rust gateway | Next: its July 2026 benchmark claims 0.7 ms p99 overhead against Bifrost's 4.5 ms |
| Helicone AI Gateway (Rust) | Same language and speed pitch |
| Portkey gateway (Node) | Popular open-source gateway |
| LiteLLM | Keep: the incumbent |

Licenses, versions, and Claude Code support of each are to verify before a run; results go in this file with their configurations.

### Gate method: the parent in alternating rounds

Adopted 2026-10-29. Until then the gate compared each commit with `bench/baseline.json`, recorded once. On 2026-10-28 it failed five times in a row on `sse_ttfb_added_p50_us` (limit 387 µs) for code identical to a commit that had passed that morning. Alternating runs showed the binary that had recorded the 205 µs baseline now measuring 406 µs on the same laptop (11 days up, under load all day); after a reboot the same code measured 264-317 µs. A stored result measures the machine's state as much as the code.

The gate now runs both commits in alternating rounds, the side that goes first alternating, and compares medians: the randomized multiple interleaved trials of [Abedi and Brecht (ICPE 2017)](https://research.spec.org/icpe_proceedings/2017/proceedings/p287.pdf), with a fixed alternation in place of a random order.

| Option | Verdict | Why |
|---|---|---|
| The parent in alternating rounds | Adopted | Both sides share the machine's state; nothing is recorded first, so it runs on any machine. Costs a second build and twice the benchmark time |
| Both at once ([duet benchmarking](https://arxiv.org/abs/2001.05811)) | Rejected | The paced and stress scenarios load every core: two runs at once would measure each other |
| A cloud VM as the gate machine | Rejected | Instances of one type differ between hosts and over time, and a VM cannot fix the clock ([Leitner and Cito](https://arxiv.org/abs/1411.2429), [Laaber et al.](https://peerj.com/preprints/3507v1.pdf)). A dedicated machine stays the option for published numbers |
| The parent's results from its own gate run | Rejected | Halves the benchmark but brings the time gap back |
| A fixed reference commit | Rejected for now | Catches regressions that add up across commits, but a metric the benchmark adds stays ungated until the reference moves, and each move needs approval. `WYE_GATE_AGAINST` runs that check on demand |

The first run was in effect an A/A run: it only changed the harness, so both sides ran the same gateway code. Across the 69 gated metrics both sides measured, the median difference was 3.8%. The largest were on small latency metrics (`auth_sse_ttfb_added_p50_us` +68%, `tls_stalled_event_added_p99_us` +63%), all within their absolute floors, and allocations differed by at most 7% (uploads).

Tolerances retuned 2026-10-29 from eight A/A comparisons: five runs on a quiet laptop, the gate's own first run, and both comparisons of a comment-only commit gated while the laptop was busy (load average 6-10, a virtual machine running). Each pair counts in both directions. Six gates were too tight; each absolute floor now covers the worst excess seen, plus about 25%:

| Gate | Floor before | Worst excess | Floor now |
|---|---|---|---|
| `sse_ttfb_added_p50_us` | 100 µs | 231 µs (busy) | 300 µs |
| `auth_sse_ttfb_added_p50_us` | 100 µs | 277 µs (busy) | 350 µs |
| `openai_sse_ttfb_added_p50_us` | 100 µs | 260 µs (busy; 143 quiet) | 350 µs |
| `sse_event_added_p50_us` | 40 µs | 122 µs (busy; under 20 quiet) | 160 µs |
| `tls_cold_unary_added_p50_us` | 5 ms | 5.3 ms (busy) | 7 ms |
| `tls_stalled_event_added_p99_us` | 2 ms | 2.0 ms (quiet) | 3 ms |

The other 90 gates kept their limits. Per-event cost stays guarded by CPU per event and zero allocations per event, which the A/A runs moved by less than their tolerances.

The new floors were not enough under everyday load. Gating this change itself, on identical gateway code with the laptop in normal use (load average 11), the run and its rerun failed on different metrics: `alloc_upload_bytes_per_request` (348 → 443 KB), then `tls_upload_added_p50_us` (3.3 → 6.3 ms). Load lands on a different metric each time, so a regression now counts only when the same metric fails both the run and the rerun. In both gate runs that failed twice on identical code, the two comparisons shared no metric.

### OpenAI stream scanners (micro-benchmark)

The gate's mock upstream speaks Anthropic Messages and OpenAI Responses, not Chat Completions; the Responses and Chat Completions usage scanners were first measured with a release micro-benchmark: 200,000 token-sized events fed one per call, best of 7, on the development laptop (2026-10-26).

| Scanner | Event | ns per event |
|---|---|---|
| Messages (gated path) | 124 B `content_block_delta` | 27 |
| Responses, first version | 173 B `response.output_text.delta` | 89 |
| Responses, whole field name matched per line | same | 34 |
| Chat, first version (searchers built per line) | 180 B content chunk | 106 |
| Chat, searchers built once per stream | same | 40 |

Lexing a 1.2 MiB Responses terminal event, or a non-streaming JSON body of that size, costs 0.56-0.61 ms per MiB, at 16 or 64 KiB chunks. Through the binary, the `openai_` benchmark variant gates the Responses path (CPU per event and per request). First runs (64-byte events, unpaced, plain path, two runs): 18.5-19.5 µs of CPU per event through Messages, 25.2-25.9 µs through Responses. Explained (2026-10-26, `wye-bench load --scenario events`, sampled with `sample`): the mock's Responses events are ~50 bytes larger than its minimal Messages events, and on the plain HTTP/1.1 path, where each event is its own write, CPU per event grows with bytes. With matched sizes (`WYE_BENCH_EVENT_BYTES=114` on Messages) the two runs read 17.1-17.3 µs for Messages against 17.6-17.9 µs for Responses: about 0.5 µs per event is the Responses scanner in a real relay, more than its 34 ns micro-benchmark (caches, split lines); the profiles are otherwise the same, `writev` first.

**Unary JSON responses (gap closed 2026-10-26).** The mock's unary replies carry no `content-type`, so the gate never runs the non-streaming usage scanner that real `application/json` replies get. With the mock sending `application/json` (a full `wye-bench run` per binary, idle machine, 2026-10-26), Wye before JSON usage scanning (f1bf78e) against Wye with it, one allocation per response: `alloc_unary_per_request` 20.6 → 21.6, `alloc_unary_bytes_per_request` 21.8 → 24.4 KB (the boxed scanner with its inline 2 KiB usage buffer), `unary_cpu_us_per_request` 120 → 124.5 (within noise), unary latency unchanged within noise. The mock now sends `application/json` on unary replies, as Anthropic and OpenAI do, and those six allocation values moved in the baseline with it (approved 2026-10-26); no other baseline value changed.

## Related work

| Work | Core idea | Fit with no shared state |
|---|---|---|
| *The Tail at Scale* | Hedged requests | Request-local; risks duplicate billing |
| Netflix `concurrency-limits` | RTT-derived adaptive admission | Needs cross-request local state |
| Maglev / rendezvous hashing | Stable selection under membership change | Deterministic; the basis of `cache_affine` |
| FrugalGPT / RouteLLM | Offline quality-aware model selection | Fits if the policy is static at runtime |
| Macaroons / Biscuit | Offline attenuated authorization | Authorization, not cumulative usage |
| Privacy Pass / offline e-cash / RLN | Redemption and double-spend limits | Need spent-token state |
| Bounded counter CRDT / escrow | Distributed numeric invariants | Need coordination ([rejected](policy.md#rejected-approaches)) |
| [TraceLab](https://arxiv.org/abs/2606.30560) (2026) | ~4,300 Claude Code and Codex sessions: long loops, long contexts, short outputs, imperfect cache hits | Benchmark workload; evidence for cache-miss diagnostics |
| [CacheWise](https://huggingface.co/papers/2606.16824.md), [Agentic Coding in the Wild](https://www.microsoft.com/en-us/research/publication/agentic-coding-in-the-wild-characterizing-github-copilot-at-production-scale/) (2026) | Agent traces: large repeated prefixes; loops coupled 1:1 with tool calls | Uploads and per-event cost matter, not bulk throughput |
| [CacheRoute](https://arxiv.org/pdf/2608.19677), [KV-cache-aware load balancing](https://www.arxiv.org/pdf/2601.18999) (2026) | Prefix-affinity routing: cache hits 64% → 93%; affinity can cost capacity | Motivates a stateless load bound on `cache_affine` |
| [R2-Router](https://arxiv.org/abs/2602.02823), [When Routing Collapses](https://arxiv.org/abs/2602.03478), [ContextualRouter](https://aclanthology.org/2026.eacl-srw.22) (2026) | Route by model and output budget; routers drift to the strongest model; kNN matches complex routers | Simple routing tables fit static config |
| [IETF Attenuating Agent Tokens](https://datatracker.ietf.org/doc/html/draft-niyikiza-oauth-attenuating-agent-tokens-00) (draft) | JWT-based offline attenuation for agent delegation | A standard format for delegable credentials |

Summaries come from abstracts; check figures against the papers before citing them.

## Feature directions

Request-local, no shared state, config in Git, analysis in the user's backend. Built: cache-miss diagnostics (2), the `policy-diff` check (5), clean stream endings (7), and the [agent-native directions](#agent-native-directions) A1-A4. Aliases and upgrade checks, also built, are in [architecture.md](architecture.md#model-aliases-and-upgrade-checks).

### 1. Agent-aware routing

Policy such as "auxiliary requests use Haiku", applied as the allowlisted `model` rewrite within one model family, keyed on Claude Code's request-class and agent-type headers. Weights fitted offline from telemetry and committed as config. Simple tables match complex routers; choosing an output budget with the model saves more than the model alone; watch for drift toward the strongest model. Needs `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` and a quality measurement per request class before any default.

### 2. Cache-miss diagnostics

Built, opt-in (`[cache_diagnostics]`, `gateway-core/src/prompt.rs`; fields in [operations.md](operations.md#telemetry)). Per request: keyed hashes of `tools` and `system` with counts, the message count, up to four `cache_control` breakpoints, and the last eight message-prefix hashes. The backend compares consecutive requests of a session in cache order (tools, system, messages); the first mismatch names the cause.

- One hash per section, not one cumulative hash: sections may arrive in any order, and cumulative hashing would need buffering.
- `cache_control` members and whitespace outside strings are excluded: Claude Code moves the marker every turn.
- xxh3 in process; every exported hash is HMAC-SHA256 under a shared key, truncated to 64 bits, so hashes compare across replicas but cannot be matched against guessed prompts. The key is required.
- Constant-memory scanner; a body it cannot follow is recorded `unknown` and forwarded unchanged. Property-tested against random bodies, whitespace, and chunkings.

Cost: the scanner runs 0.61-0.69 ms/MB on Claude Code-shaped bodies, within 15% of a naive lexer (0.54). A SIMD stage-1 prototype reached 0.17-0.24 ms/MB for classification, but a full scanner would land near 0.35-0.4 ms/MB, still above the 10% target, at the price of the first `unsafe` code on the request path. Chosen instead: per-session sampling (`sample_percent`), decided by a hash of the session id so every replica agrees.

### 3. Delegable credentials for agent swarms

A token holder mints a narrower token offline (team → developer → CI job → subagent), narrowing models, output caps, and expiry. The IETF attenuating-agent-tokens draft specifies this as JWTs, matching Wye's format. Caps each request, never cumulative spend; replay within the validity window stays possible.

### 4. Signed usage receipts

Proposal: a signed receipt per request (subject, session, model, status, reported usage, the provider's `request-id`, `prev` = hash of the previous receipt from the same process), as a compact JWS with `alg` pinned to `EdDSA`, in the usage log and OTLP. Each process keeps its own hash chain, so deletion breaks a chain without coordination; `wye receipt verify` checks signatures and gaps offline. No body hash: SHA-256 of 1 MiB costs 2.16 ms, more than Wye's whole upload cost. Cost: 13.7 µs per signature in the writer task, ~1.5% of a core at 1,000 requests/s. Attests what Wye forwarded and what the provider reported, not what it billed.

### 5. Queryable and provable policy

Beyond `wye explain` for one request: "who can reach Opus?", and a CI check that fails when a config change widens access (Zelkova-style analysis over static config). The CI check is built as `wye policy-diff` ([operations.md](operations.md#cli)): model patterns compare by coverage, ceilings and trusted signers by value. Open: the question form ("which subjects or grants reach this model?"), which needs the token side as data.

### 6. Provable cost bound

M2 ([policy.md](policy.md#provable-cost-ceiling)). Research: tighten it by subtracting non-billable framing, and measure bound ÷ actual cost.

### 7. Reliability

- Clean stream endings (built): an upstream stall or break at an event boundary ends with an Anthropic `overloaded_error` event, so Claude Code retries instead of seeing a dropped connection.
- Connect-only hedging: race a second connection attempt, never a second request.
- Deterministic simulation testing: Wye, mock, and clients in a simulated network with seeded faults.
- Proof-of-possession credentials: bind tokens to a client key.

### Agent-native directions

What Claude Code tells a gateway without exposing prompts: session id on every request, agent and parent-agent ids on subagent requests, and with `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` the request class, agent type, compaction markers, prompt id, and tool run times. Sources: Anthropic's [gateway guide](https://code.claude.com/docs/en/llm-gateway-protocol), the prompt-caching study for agents ([arXiv:2601.06007](https://arxiv.org/abs/2601.06007)).

| Direction | Stage | Result |
|---|---|---|
| Retry hygiene: Wye's 529s carry `retry-after: 1`, `x-should-retry: true` | A1 | Built |
| Model picker from policy (`GET /v1/models`) | A1 | Built |
| Spend by work type: agent headers in records and `wye.tokens` | A2 | Built; capture 0.04-0.32 µs per request, validation 0.11 µs in the writer task |
| Expected versus unexpected cache misses | A2 | Built as `cache_miss`; the first cache write of a session or a subagent reads `unexplained`, and `wye report` counts it as a session or subagent start |
| A session as one trace | A3 | Built flat: trace id from the session id, tool spans; no synthetic parent spans, since no request knows it is first. Checked in Jaeger across two replicas |
| `wye report`; tool calls from responses | A4 | Built; `content_block_start` parsed borrowed, 0.22-0.33 µs per block, zero allocations |
| Report-only secret detection | A5 | Open: needs a multi-pattern matcher and a sampled body scan. Report-only: blocking would brick a session, redaction would break caching |

Not proposed: loop detection or spend caps per session (cross-request state), response caching (shared state), prompt rewriting for cache efficiency (beyond the allowlist), blocking tool calls (agent control is a non-goal).

### Visibility for one developer

Research (2026-10-30): what a solo developer on Claude Code or Codex sees today, and what only a gateway can add without state between requests.

| For a solo developer | Strongest | What it shows | Blind spots |
|---|---|---|---|
| Overall, live | Claude Code itself ([costs](https://code.claude.com/docs/en/costs), [status line](https://code.claude.com/docs/en/statusline), [monitoring](https://code.claude.com/docs/en/monitoring-usage)) | `/usage`: session cost, main-thread cache misses with likely cause and TTL, plan bars with local attribution to skills, subagents, and MCP servers; `/context`; status line with cost, context, and 5h/7d limits; opt-in OTel events with cache writes by TTL and tool result sizes | Per session, reset by `/clear`; cache stats for the main thread only; no history without a backend |
| History, several agents | [ccusage](https://github.com/ryoppippi/ccusage) | Daily, monthly, session, and 5-hour block reports from transcripts, priced from LiteLLM's table; Codex experimental | Transcripts only: in one checked session, the 59 auxiliary requests, a compaction, and a 429 that Wye logged were missing (about 7.5% of API-equivalent spend over the whole log); no timing, tools, or prompts |
| Request bodies | [better-ccflare](https://github.com/tombii/better-ccflare), [Bifrost](https://docs.getbifrost.ai/cli-agents/claude-code) | Per-request tokens, cost, and bodies in SQLite; Bifrost groups by session | A database; no subagent, tool, or cache-miss structure documented |
| Generic gateways | Helicone, Portkey, Cloudflare AI Gateway, OpenRouter, LiteLLM | Request, token, cost, cache analytics; LiteLLM a budget status line | No Claude Code structure; dashboards and stores |

Nobody prices what-ifs for the prompt cache, the carried cost of tool output, or a per-prompt bill. Ideas that fit the design (work in the log writer task or an offline CLI, nothing on the request path), ranked by value to one developer times feasibility; all are research until built:

1. **Cache waste and TTL advice, main thread and subagents.** "Subagent cache writes expired unread: $X; a 1h TTL would save $Y" (Claude Code lets users set `promptCacheTtl` and `subagentPromptCacheTtl`). Needs the 5m and 1h cache-write split in records, which the usage scanner already parses; the rest is offline.
2. **Carried cost of tool output.** The growth of a conversation's prompt between requests, attributed to the tools that ran in between (the tool-time hint), times the requests that re-read it until compaction, at the cache-read price: "Bash output re-read 180 times cost $41." Offline, from fields logged today.
3. **A per-prompt bill with labels.** Main, subagent, and auxiliary requests rolled up per prompt, labeled with the prompt's first line by joining Claude Code's local transcripts (which carry `promptId`) in the CLI, so prompt text never enters the log.
4. **`wye statusline`.** Reads the end of the log for the status line's session and prompt: this prompt's cost with its subagents and retries, the last cache rebuild, the costliest carried tool. The delivery channel for 1-3.
5. **Complete API-equivalent spend.** Subscription value at API prices including what transcripts miss; prices applied at report time would price old logs too (a log written without `[pricing]` has no `cost_usd`).
6. **Failure ledger.** 429, 529, and 5xx answers, cancellations, and time lost, with the expected session-start probe separated out.

Not worth building: live 5h/7d headroom, which Claude Code already reads from the headers Wye forwards. Unverified: the status line's `rate_limits` behind Wye (likely, for the same reason).

### Excluded: needs shared state

Semantic or response caching, global rate limits, redemption-based tokens, adaptive per-node concurrency ([policy.md](policy.md#rejected-approaches)).

## Performance techniques

Each is a hypothesis until `wye-bench` measures it.

| Technique | Status |
|---|---|
| HTTP/2 to the provider | In effect: negotiated by ALPN (verified against `api.anthropic.com`) |
| Spread streams over 64 connections | Adopted: TLS unary added latency -80%, CPU per MB -66%, throughput 4×; RSS after load 10 → 30 MB |
| 256 lanes instead of 64 | Rejected (2026-10-24, two alternating 20 s paced runs): CPU per event +3.5% and +8%, idle RSS +18%. A profile of the paced load had shown ~10% of busy samples waiting on h2's per-connection lock, but more connections cost more than the contention they remove |
| Maximum HTTP/2 connection window, 256 KiB per stream | Adopted (bug fix, 2026-10-24): with hyper's defaults (5 MiB connection, 2 MiB stream) 200 stalled clients made 643-811 other streams fail; now 0. RSS per stalled stream 3.3 MB (defaults) → 2.0 MB (1 MiB, first fix) → 0.49 MB; bulk CPU per MB relayed +10% against 1 MiB (4.06 → 4.47 ms), token streams unaffected |
| Client write-stall timeout | Adopted: a connection whose writes make no progress for `client_write_timeout_secs` (60) is closed, which ends a stalled stream; zero cost while writes never block (the timer is allocated at the first blocked write) |
| Per-connection token cache | Adopted: CPU per authenticated request -38% and -44% in two A/B runs, equal to unauthenticated; ~0.3 KB per connection |
| Shared cached identity (`Arc`) and borrowed log line | Adopted: allocations per authenticated request 37.5 → 20.6 |
| 32 KiB client-connection buffer cap | Adopted (approved tradeoff): idle TLS stream after 512 KB 150 → 49 KB; plain uploads +7% CPU; 16 KiB cost +50% plain upload CPU |
| Client socket send buffer fixed at 128 KiB | Adopted (2026-10-29): autotuning let the kernel send buffer of a client that stopped reading grow to 4 MiB, memory outside Wye's own bounds; now 32 MB for 256 stalled clients (64 MB on Linux, which doubles the value set). Found when the stalled-client test exhausted a 7 GB macOS CI runner's 96 MB network pool. A/B, three alternating rounds: bulk CPU per MB +7.6% TLS (within its noise), -1.3% plain; per-request and per-event costs unchanged within noise |
| Boxed request-body state | Adopted: holding 88 bytes inline cost +2.8 KB per idle plain stream (hyper stores 32 requests per connection inline) |
| SIMD `memchr` usage scanning | Adopted: byte loop +49% CPU per MB (blocked); `memchr` +5.8% |
| Forced inlining of the prompt lexer | Adopted: ~20% faster scanner |
| Fast cancellation as a metric | Gated: release ~80 µs p50, < 0.2 ms max |
| Adaptive write coalescing | Rejected: no gain |
| Lazy idle deadline | Rejected: no gain |
| SWAR string-end search | Rejected: no gain over a byte loop |
| mimalloc | Rejected: idle memory +9%, RSS +12%, CPU per MB +4% |
| Failover: copy kept bodies into one buffer per request | Rejected (2026-10-26, `failover_` variant, two runs): keeping hyper's chunks costs allocations because hyper cannot reuse its read buffer (99.5 per agent upload vs 40.5 without failover), so copying into one buffer reserved from the declared length was tried. Allocations fell to 58.5, but CPU per MB rose 1.86 → 2.08 ms (agent upload) and 1.81 → 1.97 ms (1 MiB upload), heap bytes 1.12 → 1.55 MB per agent upload, and +1 allocation per unary request. The kept chunks stay; failover's upload cost (+25% CPU per MB over plain) is the price of resending without a copy |
| Fat LTO, one codegen unit | Not adopted: CPU per MB 6-8% lower, but worse event p99 in both A/B runs, 2× build time |
| Thin LTO | Not adopted (2026-10-30, two A/B runs of 3 alternating rounds): CPU 2-6% lower on nearly every CPU metric in both runs (per SSE event -4.7% / -5.5%, TLS unary per request -5.0% / -3.3%, bulk per MB -5.6% / -3.8%), below the gate's sensitivity, and no event p99 penalty as with fat LTO. But CPU per MB on cache-diagnostics uploads rose in both runs (+5.9% / +2.8%), and a release rebuild after a `gateway-core` change takes 26-32 s instead of 18.5 s, which every gate run pays several times. Adopting it needs that tradeoff approved |
| One h2 lock per frame | Candidate: A/B inconclusive (-5%, +12%) |
| Fewer runtime threads | Deployment knob (`TOKIO_WORKER_THREADS`): CPU per MB 3.6 → 1.7 ms from 16 to 1 thread, latency under load rises. 2026-10-25, TLS, single runs, 16 / 4 / 2 workers: per-event delay p50 318 / 266 / 305 µs, CPU per paced event 38 / 25 / 18 µs, but unary added p50 237 / 690 / 823 µs |
| Pre-warmed connections | Low value: 32 first requests on a new process add 3.6-8.4 ms p50, mostly first execution of the TLS and h2 code; after one warm-up request ~2 ms (concurrent handshakes), and a single cold connection adds nothing |
| kTLS + splice; thread-per-core runtime | Research only |
| Transparent stream repair | Rejected: changes semantics and billing; Claude Code retries |
