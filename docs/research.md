# Research

Measured results, open questions, and directions; nothing here is a product promise. Unless noted, numbers come from one laptop (i9-9880H, 16 threads, macOS), loopback, a mock upstream, release build, median of 3 runs: lab results, not production.

## Questions to test

- What TTFT and inter-event latency does Wye add over direct access, under load and with slow readers?
- How fast does a disconnect release upstream work, and does the provider stop generation or billing?
- How much does cache-affine selection raise cache hit rate and lower billed cost on real agent workloads?
- How loose is the provable cost bound (bound ÷ actual cost) on real sessions?
- Which results hold only against the mock?

A proxy adds a hop; it does not improve latency over direct access. Local cancellation does not prove the provider stopped billing. Real-provider experiments are opt-in and record model, usage, cache behavior, pricing, and billed cost.

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
| Bedrock's Messages endpoint and Claude Platform on AWS accept SigV4 with `UNSIGNED-PAYLOAD` | Signing streamed bodies ([M4](architecture.md#claude-across-providers-planned-m4)) | One signed request per endpoint |
| Vertex's streaming URL and framing; `count_tokens` and `anthropic-beta` on each Claude provider | M4 | Platform docs; one recorded stream per provider |
| `429`/`529` from each Claude provider means the request was not processed | Failover without duplicate billing | Provider docs; billing records |
| Each upgrade-check rule (a field a model rejects) | [Upgrade checks](architecture.md#model-aliases-and-upgrade-checks) | Built from Anthropic's migration guide (2026-10-26); one opt-in live request per rule still open |

## Benchmarks

**Method.** `wye-bench` runs each scenario directly against a mock Anthropic upstream (unary JSON, timestamped SSE) and through a `wye` child process, and reports what Wye adds. Two upstream paths: plain HTTP/1.1 (low noise, best for catching regressions) and TLS + HTTP/2 (the production shape, `tls_` metrics). Variants on the plain path: `auth_`, `models_`, `diag_`, `otlp_`, and `openai_` (Wye on `[routes.openai]`, the mock framing OpenAI Responses). An allocation-counting build adds `alloc_`. The commit gate compares every commit with `bench/baseline.json` ([development.md](development.md#commit-gate)). Noise: 10–15% on CPU per MB, ±25% on the TLS path, more on tails; the gate catches ~25% or ~100 µs.

Fair comparisons need identical hardware, payloads, keep-alive and TLS settings, upstream behavior, and features, and record hardware, build, workload, samples, percentiles, and errors. A gateway with more middleware enabled is not a fair baseline unless the difference is stated.

### Current harness and baseline

| Metric | Plain HTTP/1.1 | TLS + HTTP/2 |
|---|---|---|
| Added unary latency p50 / p99 | 222 / 263 µs | 260 µs / not gated |
| Added time to first byte p50 | 223 µs | 299 µs |
| Added delay per SSE event p50 / p99 | 145 / 326 µs | 181 / 248 µs |
| CPU per unary request | 112 µs | 156 µs |
| CPU per streamed MB | 3.2 ms | 4.1 ms |
| Memory per idle stream (RSS) | 28 KB | 5.6 KB |
| Upstream release after disconnect, max | 157 µs | 99 µs |
| 1 MiB upload: added p50, CPU per MB | 4.1 ms, 1.45 ms | 4.5 ms, 2.9 ms |
| 2,000 token-paced streams: event delay p50 / p99 | | 1.8 / 4.9 ms |
| 2,000 token-paced streams: CPU per event, memory per stream | | 36 µs, 9.5 KB |
| Bulk throughput (direct 3.85 / 1.62 GB/s) | 1.26 GB/s | 1.09 GB/s |

Heap allocations (`alloc_`, plain path; spread under 1%, uploads ~5%):

| Metric | Value |
|---|---|
| Per streamed SSE event | 0 |
| Per stream setup | 23 |
| Per unary request, plain and authenticated | 20.6 (21.6 KB) |
| Live heap per idle stream | 49 KB plain, 23 KB TLS |
| Live heap per idle stream after a 512 KB request | 77 KB plain, 48 KB TLS (150 KB before the 32 KiB buffer cap) |
| Per 1 MiB upload | 44 allocations, 0.43 MB (1.27 MB before the cap) |
| Live heap per stalled stream, TLS | 0.56 MB with a 256 KiB upstream window, 2.2 MB with 1 MiB: about twice the window, independent of streams per connection |

Variants (plain path):

| Variant | Cost against plain Wye |
|---|---|
| `auth_`: Ed25519 credentials | +88 µs CPU per full verification (the check itself 55–62 µs); on a reused connection the per-connection cache makes it free |
| `models_`: model permissions | +1 allocation per request; +0.67 ms/MB on Claude Code-shaped uploads (the repeated-`model` check lexes the rest of the body); free on one-string bodies |
| `diag_`: cache diagnostics | +2 allocations per request; +48% upload CPU on Claude Code-shaped bodies (2.75 vs 1.86 ms/MB), hence per-session sampling |
| `otlp_`: OTLP export | within noise (118.5 vs 120 µs CPU per request) |

**Stress** (TLS path, median of 3, 2026-10-24). 1,000 paced streams while 1 MiB uploads run 8 at a time: events +1.0 / 1.7 ms p50 / p99, uploads +2.4 ms p50. Beside 200 clients that stopped reading: events +2.7 ms p99, 0.49 MB RSS per stalled stream with the 256 KiB upstream window (2.0 MB with 1 MiB), no failures; RSS stays at its peak after they close, because the allocator keeps the freed pages. 32 first requests on a new process: +3.2 ms p50 ([pre-warming](#performance-techniques)).

**Soak** (`wye-bench load --scenario paced --seconds 150`, alloc-stats build): 34 batches of 2,000 paced streams, 6.8M events, zero failures, 34 µs CPU per event. Live heap rose ~600 KB over the first ~20 batches (lanes and buffers warming), then held at 49,363 KB; RSS levelled near 56 MB. No growth per stream served. A multi-hour soak is open: the overnight attempt (2026-10-18) was void because the laptop slept.

**Capacity per core** (`--scenario capacity`, one Tokio worker, TLS + HTTP/2, 50 events/s of 100 B per stream, 2 runs):

| Streams | Events/s | Wye cores | CPU µs/event | Direct p99 | Added p99 |
|---|---|---|---|---|---|
| 500 | 11,900 | 0.24 | 20 | 0.9–1.1 ms | 4.1–4.4 ms |
| 1,000 | 23,900 | 0.37–0.39 | 16 | 1.5–2.2 ms | 6.0–6.4 ms |
| 1,500 | 35,800–35,900 | 0.46–0.48 | 13 | 2.6–3.0 ms | 8.3–8.9 ms |
| 2,000 | 47,900 | 0.57–0.58 | 12 | 3.2–3.3 ms | 12.1–13.0 ms |
| 3,000 | 70,500–71,300 | 0.71–0.73 | 10 | 4.1–4.3 ms | 67–79 ms |
| 4,000 | 88,400–91,000 | 0.74–0.76 | 8 | 5.2–5.3 ms | 219–258 ms |

Sustainable (≤10 ms added p99): 1,500 streams, ~36,000 events/s per worker. The limit is queueing on the thread, not CPU. Two workers held 3,000 streams (linear); four were limited by the laptop's load generator (4–5 cores at 3,000 streams), so multi-core capacity needs a separate load machine. CPU per event rises with worker count (12–20 µs at 1, 14–27 at 2, 21–38 at 4): cross-thread wakeups. A smaller default worker count is worth measuring on a real host.

**Profiles.** Plain path: almost all system calls (`writev`, `recvfrom`). TLS path: first dominated by h2's per-connection mutex; spreading streams over 64 connections ([architecture.md](architecture.md#upstream-connections)) cut CPU per MB 12.3 → 4.1 ms and unary added latency 1.5 → 0.3 ms. What remains: relaying each frame as it arrives, and handing each chunk between the client and upstream connection tasks. Paced load, TLS, 2026-10-24 (`sample`, 10 s of 2,000 streams): of busy samples, `writev` to clients 43% (one write per event, inherent at token pace), `recvfrom` from the upstream 16%, waits on h2's per-connection lock 10% (`poll_data`, `is_end_stream`), Wye's own code under 1%. Memory per stream, 2026-10-25 (`heap` and `malloc_history`, 2,000 paced TLS streams, `MallocStackLogging=1`): of the ~24 KB of heap per stream, hyper's HTTP/1.1 client connection holds a fixed 8.5 KB read buffer and an 8.5 KB write buffer (70%), its dispatcher 3.5 KB, the Tokio task 0.9 KB, request headers 0.7 KB, timer and service closure 0.2 KB; Wye's own state is not among the top sites. hyper 1.x exposes only a cap on those buffers (`max_buf_size`, set to 32 KiB), not their initial size, so shrinking them needs a hyper change.

**Compressed responses** (2026-10-17, first real session): every stream was recorded `usage_status: unknown`. Claude Code asks for `gzip, deflate, br, zstd`; Anthropic answers Brotli; the mock never compresses, so no test caught it. Fix: ask the upstream for `identity`. Its cost, from 353 real responses rebuilt as streams and compressed with a flush per event:

| Delta size | Identity per response | Per event | gzip -6 | Brotli q4 / q11 | zstd -3 |
|---|---|---|---|---|---|
| 8 B | 35.9 KB | 151 B | 4.0× | 5.7× / 5.8× | 5.8× |
| 16 B | 21.5 KB | 173 B | 3.5× | 4.5× / 4.6× | 4.6× |
| 32 B | 14.3 KB | 211 B | 3.1× | 3.6× / 3.7× | 3.7× |

Identity costs 10–30 KB more per response, ~28 kbit/s while streaming: 0.12 ms per event on a 10 Mbit/s link against a 20 ms token gap. Decompressing a copy instead costs 6–11 µs per event (20–90% of Wye's relay cost), needs an attacker-sized decoder window per stream, and ends zero allocations per event. Rejected.

**TLS uploads** (open gap: KrakenD 2.0 vs Wye 2.6–3.1 ms CPU per MB). Like for like over HTTP/1.1, Wye is ~10% behind; HTTP/2 adds ~9% more. Of HTTP/2 busy samples: 45% rustls `writev`, 16% AES-GCM, 15% `recvfrom`, 10% the h2 mutex; h2 flushes one 16 KiB frame at a time (64 KiB frames: 2.59 ms; 256 KiB: 2.29 ms). Anthropic advertises 16 MiB frames but a 64 KiB stream window, which may limit an upload to window ÷ RTT (~2 MB/s at 30 ms) unless the server grows it; `scripts/upload-latency.sh` measures that against the real API (needs a key). The default stays HTTP/2 until it does.

### Against KrakenD

KrakenD 3.0.1 CE in pass-through (`no-op` encoding) at its best case (no access log); Wye keeps its usage log. Single run, all 16 threads, 2026-10-15.

**KrakenD CE cannot stream SSE:** `no-op` buffers the whole response (an event sent at 0 s arrived at 3 s); `sse` encoding is Enterprise-only.

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
- **Paced event p50** (1.6 vs 2.1 ms, this run only): Bifrost queues stream starts (time to first byte p99 0.99 s), so fewer of the 2,000 streams overlap and those running see less contention; its p99 is 58 ms. In the other two runs its paced p50 was 129 and 159 ms.
- **Per-event delay p50** is a tie within noise: Wye's swung from 195 to 375 µs between identical runs, Bifrost's from 190 to 272 µs. Bifrost's upstream is HTTP/1.1 and the direct baseline is HTTP/2, so an HTTP/1.1 relay also skips the mock's HTTP/2 framing. With Wye on HTTP/1.1 as well (`http_version = "1.1"`): 217 vs 243 µs p50, 236 vs 1,502 µs p99. Wye keeps HTTP/2 as the default because one connection per stream costs more under load (paced event p50 3.4 ms on HTTP/1.1).
- **Memory per paced stream** reads lower (5 vs 9.6 KB), but from an idle process 20 times larger.

Load generator, mock, and gateway share one laptop (8 physical cores); at 2,000 paced streams Wye alone uses ~3.7 cores and Bifrost all 16 threads, so paced latencies include machine contention. Bifrost's published "11 µs" counts only its internal stages ("excludes JSON marshalling and HTTP calls"); the numbers here are end to end, what a client sees.

#### Comparison runs

Competitors run with `--bifrost`, `--litellm`, or `--krakend`. Gateways that parse requests and events need what real traffic always has, so comparison runs differ from gate runs in two ways, through every gateway alike:
- streams are complete Anthropic Messages streams (`message_start`, `text_delta` deltas carrying the send time in an extra field, `message_stop`); Bifrost drops events that are not Anthropic-shaped;
- the Claude Code-shaped upload says `"stream":false`, since the upload scenarios get a unary reply.

All runs send `"stream":true` on streaming requests and name the model `anthropic/bench`. Bifrost needs two more things: the mock's control headers go as `x-bf-eh-<name>`, the only extra headers it forwards, and requests leave `anthropic-version` to it, because its Anthropic route drops forwarded headers when the request carries one.

### Benchmark suite: kept, cut, missing

The suite measures the gateway's own cost under agent-shaped load; each part earns its run time or goes. Approved 2026-10-24; the stress scenarios are built, the competitors are next (M5).

| Part | Verdict | Why |
|---|---|---|
| TLS + HTTP/2 path, paced streams, uploads, idle memory, cancel, allocations | Keep | The production shape and the costs agents feel |
| Plain HTTP/1.1 path | Keep | Lowest noise: catches regressions the TLS path hides |
| `auth_`, `models_`, `diag_` variants | Keep | Each guards a per-request or per-MB cost of a feature |
| `otlp_` variant | Gated | Export is on in every team deployment; reported-only metrics regress silently |
| Bulk throughput (MB/s) | Report, stop gating MB/s | Least relevant to token-paced traffic; CPU per MB stays gated |
| KrakenD runs | Retire from routine runs | Cannot stream SSE on CE; the recorded result above stands |
| Mixed load (`mixed_`) | Built | Uploads and paced streams at once: head-of-line effects on event p99 when agents send MB contexts |
| Cold connections (`cold_`) | Built | First requests on a new process: connects and TLS handshakes |
| Stalled clients (`stalled_`) | Built | Memory and event delay beside readers that stopped; found the shared-window stall on its first run |
| Missing: agent trace replay | Add (M5) | [TraceLab](#related-work) context sizes, output lengths, turn gaps |
| Missing: floor baseline | Add | Wye against a dumb streaming proxy shows what its features cost |
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


### OpenAI stream scanners (micro-benchmark)

The gate's mock upstream speaks Anthropic only, so the Responses and Chat Completions usage scanners are measured with a release micro-benchmark: 200,000 token-sized events fed one per call, best of 7, on the baseline laptop (2026-10-26).

| Scanner | Event | ns per event |
|---|---|---|
| Messages (gated path) | 124 B `content_block_delta` | 27 |
| Responses, first version | 173 B `response.output_text.delta` | 89 |
| Responses, whole field name matched per line | same | 34 |
| Chat, first version (searchers built per line) | 180 B content chunk | 106 |
| Chat, searchers built once per stream | same | 40 |

Lexing a 1.2 MiB Responses terminal event, or a non-streaming JSON body of that size, costs 0.56–0.61 ms per MiB, at 16 or 64 KiB chunks. Through the binary, the `openai_` benchmark variant gates the Responses path (CPU per event and per request). First runs (64-byte events, unpaced, plain path, two runs): 18.5–19.5 µs of CPU per event through Messages, 25.2–25.9 µs through Responses; the scanners explain ~30 ns of that gap, the rest is not yet explained (open).

**Coverage gap: unary JSON responses.** The mock's unary replies carry no `content-type`, so the gate never runs the non-streaming usage scanner that real `application/json` replies get. With the mock sending `application/json` (a full `wye-bench run` per binary, idle machine, 2026-10-26), Wye before JSON usage scanning (3fe2d4c) against Wye with it, one allocation per response: `alloc_unary_per_request` 20.6 → 21.6, `alloc_unary_bytes_per_request` 21.8 → 24.4 KB (the boxed scanner with its inline 2 KiB usage buffer), `unary_cpu_us_per_request` 120 → 124.5 (within noise), unary latency unchanged within noise. The mock now sends `application/json` on unary replies, as Anthropic and OpenAI do, and those six allocation values moved in the baseline with it (approved 2026-10-26); no other baseline value changed.
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

Request-local, no shared state, config in Git, analysis in the user's backend. Built: cache-miss diagnostics (2), clean stream endings (7), and the [agent-native directions](#agent-native-directions) A1–A4. Aliases and upgrade checks are designed in [architecture.md](architecture.md#model-aliases-and-upgrade-checks).

### 1. Agent-aware routing

Policy such as "auxiliary requests use Haiku", applied as the allowlisted `model` rewrite within one model family, keyed on Claude Code's request-class and agent-type headers. Weights fitted offline from telemetry and committed as config. Simple tables match complex routers; choosing an output budget with the model saves more than the model alone; watch for drift toward the strongest model. Needs `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` and a quality measurement per request class before any default.

### 2. Cache-miss diagnostics

**Built**, opt-in (`[cache_diagnostics]`, `gateway-core/src/prompt.rs`; fields in [operations.md](operations.md#telemetry)). Per request: keyed hashes of `tools` and `system` with counts, the message count, up to four `cache_control` breakpoints, and the last eight message-prefix hashes. The backend compares consecutive requests of a session in cache order (tools, system, messages); the first mismatch names the cause.

- One hash per section, not one cumulative hash: sections may arrive in any order, and cumulative hashing would need buffering.
- `cache_control` members and whitespace outside strings are excluded: Claude Code moves the marker every turn.
- xxh3 in process; every exported hash is HMAC-SHA256 under a shared key, truncated to 64 bits, so hashes compare across replicas but cannot be matched against guessed prompts. The key is required.
- Constant-memory scanner; a body it cannot follow is recorded `unknown` and forwarded unchanged. Property-tested against random bodies, whitespace, and chunkings.

Cost: the scanner runs 0.61–0.69 ms/MB on Claude Code-shaped bodies, within 15% of a naive lexer (0.54). A SIMD stage-1 prototype reached 0.17–0.24 ms/MB for classification, but a full scanner would land near 0.35–0.4 ms/MB, still above the 10% target, at the price of the first `unsafe` code on the request path. Chosen instead: per-session sampling (`sample_percent`), decided by a hash of the session id so every replica agrees.

### 3. Delegable credentials for agent swarms

A token holder mints a narrower token offline (team → developer → CI job → subagent), narrowing models, output caps, and expiry. The IETF attenuating-agent-tokens draft specifies this as JWTs, matching Wye's format. Caps each request, never cumulative spend; replay within the validity window stays possible.

### 4. Signed usage receipts

Proposal: a signed receipt per request (subject, session, model, status, reported usage, the provider's `request-id`, `prev` = hash of the previous receipt from the same process), as a compact JWS with `alg` pinned to `EdDSA`, in the usage log and OTLP. Each process keeps its own hash chain, so deletion breaks a chain without coordination; `wye receipt verify` checks signatures and gaps offline. No body hash: SHA-256 of 1 MiB costs 2.16 ms, more than Wye's whole upload cost. Cost: 13.7 µs per signature in the writer task, ~1.5% of a core at 1,000 requests/s. Attests what Wye forwarded and what the provider reported, not what it billed.

### 5. Queryable and provable policy

Beyond `wye explain` for one request: "who can reach Opus?", and a CI check that fails when a config change widens access (Zelkova-style analysis over static config).

### 6. Provable cost bound

M2 ([policy.md](policy.md#provable-cost-ceiling)). Research: tighten it by subtracting non-billable framing, and measure bound ÷ actual cost.

### 7. Reliability

- **Clean stream endings** (built): an upstream stall or break at an event boundary ends with an Anthropic `overloaded_error` event, so Claude Code retries instead of seeing a dropped connection.
- **Connect-only hedging:** race a second connection attempt, never a second request.
- **Deterministic simulation testing:** Wye, mock, and clients in a simulated network with seeded faults.
- **Proof-of-possession credentials:** bind tokens to a client key.

### Agent-native directions

What Claude Code tells a gateway without exposing prompts: session id on every request, agent and parent-agent ids on subagent requests, and with `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` the request class, agent type, compaction markers, prompt id, and tool run times. Sources: Anthropic's [gateway guide](https://code.claude.com/docs/en/llm-gateway-protocol), the prompt-caching study for agents ([arXiv:2601.06007](https://arxiv.org/abs/2601.06007)).

| Direction | Stage | Result |
|---|---|---|
| Retry hygiene: Wye's 529s carry `retry-after: 1`, `x-should-retry: true` | A1 | Built |
| Model picker from policy (`GET /v1/models`) | A1 | Built |
| Spend by work type: agent headers in records and `wye.tokens` | A2 | Built; capture 0.04–0.32 µs per request, validation 0.11 µs in the writer task |
| Expected versus unexpected cache misses | A2 | Built as `cache_miss`; a session's first request reads `unexplained` and is excluded by session |
| A session as one trace | A3 | Built flat: trace id from the session id, tool spans; no synthetic parent spans, since no request knows it is first. Checked in Jaeger across two replicas |
| `wye report`; tool calls from responses | A4 | Built; `content_block_start` parsed borrowed, 0.22–0.33 µs per block, zero allocations |
| Report-only secret detection | A5 | Open: needs a multi-pattern matcher and a sampled body scan. Report-only: blocking would brick a session, redaction would break caching |

Not proposed: loop detection or spend caps per session (cross-request state), response caching (shared state), prompt rewriting for cache efficiency (beyond the allowlist), blocking tool calls (agent control is a non-goal).

### Excluded: needs shared state

Semantic or response caching, global rate limits, redemption-based tokens, adaptive per-node concurrency ([policy.md](policy.md#rejected-approaches)).

## Performance techniques

Each is a hypothesis until `wye-bench` measures it.

| Technique | Status |
|---|---|
| HTTP/2 to the provider | **In effect**: negotiated by ALPN (verified against `api.anthropic.com`) |
| Spread streams over 64 connections | **Adopted**: TLS unary added latency −80%, CPU per MB −66%, throughput 4×; RSS after load 10 → 30 MB |
| 256 lanes instead of 64 | **Rejected** (2026-10-24, two alternating 20 s paced runs): CPU per event +3.5% and +8%, idle RSS +18%. A profile of the paced load had shown ~10% of busy samples waiting on h2's per-connection lock, but more connections cost more than the contention they remove |
| Unbounded HTTP/2 connection window, 256 KiB per stream | **Adopted** (bug fix, 2026-10-24): with hyper's defaults (5 MiB connection, 2 MiB stream) 200 stalled clients made 643–811 other streams fail; now 0. RSS per stalled stream 3.3 MB (defaults) → 2.0 MB (1 MiB, first fix) → 0.49 MB; bulk CPU per MB relayed +10% against 1 MiB (4.06 → 4.47 ms), token streams unaffected |
| Client write-stall timeout | **Adopted**: a connection whose writes make no progress for `client_write_timeout_secs` (60) is closed, which ends a stalled stream; zero cost while writes never block (the timer is allocated at the first blocked write) |
| Per-connection credential cache | **Adopted**: CPU per authenticated request −38% and −44% in two A/B runs, equal to unauthenticated; ~0.3 KB per connection |
| Shared cached identity (`Arc`) and borrowed log line | **Adopted**: allocations per authenticated request 37.5 → 20.6 |
| 32 KiB client-connection buffer cap | **Adopted** (approved tradeoff): idle TLS stream after 512 KB 150 → 48 KB; plain uploads +7% CPU; 16 KiB cost +50% plain upload CPU |
| Boxed request-body state | **Adopted**: holding 88 bytes inline cost +2.8 KB per idle plain stream (hyper stores 32 requests per connection inline) |
| SIMD `memchr` usage scanning | **Adopted**: byte loop +49% CPU per MB (blocked); `memchr` +5.8% |
| Forced inlining of the prompt lexer | **Adopted**: ~20% faster scanner |
| Fast cancellation as a metric | **Gated**: release ~80 µs p50, < 0.2 ms max |
| Adaptive write coalescing | **Rejected**: no gain |
| Lazy idle deadline | **Rejected**: no gain |
| SWAR string-end search | **Rejected**: no gain over a byte loop |
| mimalloc | **Rejected**: idle memory +9%, RSS +12%, CPU per MB +4% |
| Fat LTO, one codegen unit | **Not adopted**: CPU per MB −6–8%, worse event p99 in both A/B runs, 2× build time |
| One h2 lock per frame | **Candidate**: A/B inconclusive (−5%, +12%) |
| Fewer runtime threads | **Deployment knob** (`TOKIO_WORKER_THREADS`): CPU per MB 3.6 → 1.7 ms from 16 to 1 thread, latency under load rises. 2026-10-25, TLS, single runs, 16 / 4 / 2 workers: per-event delay p50 318 / 266 / 305 µs, CPU per paced event 38 / 25 / 18 µs, but unary added p50 237 / 690 / 823 µs |
| Pre-warmed connections | **Low value**: 32 first requests on a new process add 3.6–8.4 ms p50, mostly first execution of the TLS and h2 code; after one warm-up request ~2 ms (concurrent handshakes), and a single cold connection adds nothing |
| kTLS + splice; thread-per-core runtime | Research only |
| Transparent stream repair | **Rejected**: changes semantics and billing; Claude Code retries |
