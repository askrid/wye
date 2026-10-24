# Research

Open questions, measurement methodology, and research directions. Nothing here is a product promise.

## Core question

How much can a stateless inference proxy improve streaming tail latency and resource efficiency through routing, backpressure, and cancellation, without cross-request coordination or higher inference cost?

Status: open, not an established result.

## Questions to test

- What TTFT and inter-event latency does Wye add relative to direct upstream access?
- Under concurrent streams and slow readers, how do backpressure and bounded buffering affect tail latency, CPU, and RSS?
- How quickly does a downstream disconnect release local and upstream resources? Does the provider stop generation or billing?
- Once routing exists, can it select a lower-latency eligible provider without increasing measured cost for a comparable workload?
- How much does cache-affine routing raise cache hit rate and lower billed cost, compared with cost-only selection, on real agent workloads?
- How loose is the provable cost bound in practice (bound ÷ actual cost) across typical requests?
- Which results hold against a controlled mock upstream, and which depend on provider behavior?

## Measurement boundaries

A proxy adds a network and processing step; do not assume it improves latency over direct access. Evaluate resource-control improvements against a clearly specified buffering or cancellation baseline, and measure direct-access overhead separately.

Local cancellation does not prove provider-side generation or billing stopped; that needs provider telemetry or billing evidence. Ambiguous failures and retries may incur duplicate cost.

Real-provider experiments are opt-in and record model, provider, input/output usage, cache behavior, pricing assumptions, and billed cost where available. Different models are not automatically comparable in quality or tokenization.

## Verification needed

Design decisions that depend on provider facts not yet checked. Each must be confirmed, with sources recorded, before the dependent feature ships.

| Assumption | Depends on it | How to check |
|---|---|---|
| Input tokens ≤ UTF-8 bytes for each provider tokenizer, including special and template tokens added around messages | [Provable cost ceiling](policy.md#provable-cost-ceiling) | Tokenizer construction (byte-level BPE, byte fallback); adversarial fuzzing against provider usage reports; document per-message overhead tokens and add them to the bound |
| Output-token limit fields cap reasoning tokens, per model | Provable cost ceiling | Provider docs; requests with low limits and reasoning enabled, compare reported usage |
| Which providers enforce project/workspace spend limits as hard stops, and with what lag | [Provider-held budgets](policy.md#provider-held-budgets) | Provider docs; a low-limit test project driven past its limit |
| Claude Code gateway contract for each targeted release (endpoints, open header and body lists, response headers) | M0 | [Gateway compatibility guide](https://code.claude.com/docs/en/llm-gateway-protocol); contract tests against each targeted release |
| A process-environment `ANTHROPIC_BASE_URL` overrides an `ANTHROPIC_BASE_URL` set in the user's Claude Code settings | `wye claude` wrapper | Launch with both set; observe which endpoint receives traffic |
| Anthropic per-image token maximum after resizing, per model | Image pricing in the cost ceiling | Anthropic vision docs; requests with maximal images, compare reported input tokens |
| Bound ÷ actual cost on real Claude Code sessions with prompt caching | Positioning the ceiling as a guard | Opt-in telemetry from real sessions |
| Anthropic tool-use system-prompt overhead and prompt-length price tiers per model | Provable cost ceiling | Anthropic pricing and tool-use docs; compare reported input tokens with and without `tools` |
| Current Codex transport and configuration for Responses (HTTP SSE vs. other transports; custom base URL support) | M3 | Codex source and docs for the target version |
| Share of `auxiliary`, `compaction`, and `subagent` requests in real Claude Code sessions, and quality impact of routing each to a smaller Claude model | [Agent-aware routing](#1-agent-aware-routing) | Opt-in telemetry with gateway hint headers enabled; task-level evaluation |
| Whether Bedrock's Messages endpoint and Claude Platform on AWS accept SigV4 with an unsigned payload hash (`UNSIGNED-PAYLOAD`) | Signing streamed bodies without buffering them ([architecture.md](architecture.md#claude-across-providers-planned-m4)) | AWS service docs; a signed request with an unsigned payload against each endpoint |
| Vertex's streaming URL (`:streamRawPredict`) and that its stream is Anthropic SSE; `count_tokens` paths on Bedrock, Vertex, and Claude Platform on AWS; where each puts `anthropic-beta` | Claude across providers | Platform docs; one recorded stream and request per provider |
| Whether `429`/`529` from each Claude provider always means the request was not processed (safe to fail over) | Failover without duplicate billing | Provider docs on rate limits and overload; billing records for refused requests |
| Each upgrade-check rule (a field a model rejects) against the provider's migration notes and a real `400` | [Upgrade checks](architecture.md#model-aliases-and-upgrade-checks-planned): a wrong rule reports false failures | Provider migration guides; one opt-in request per rule and model |
| Which SDKs tolerate the extra empty-`choices` usage chunk | `include_usage` injection default | SDK conformance tests in Python, TypeScript, Go, and common agent frameworks |

## Benchmarks

**Mock upstream first:** fixed event timing and payloads, varied concurrency, slow readers, deliberate disconnects.

- **Baselines:** direct mock upstream, Wye, LiteLLM, and KrakenD (measured); newer lean gateways (Bifrost, Helicone's gateway, Portkey) next, each only in a feature-equivalent streaming configuration.
- **Workloads:** small unary requests; SSE with controllable first-token delay and token rate; 1k/10k concurrent streams as stress targets; mixed fast/slow consumers; disconnect storms; upstream failures; TLS on/off where fair.
- **Metrics:** throughput; p50/p95/p99 added latency; TTFT overhead; chunk propagation delay; RSS per active stream; CPU per request and per streamed byte; dropped/cancelled requests; leak checks; infrastructure cost at equivalent load.
- **Controls:** identical hardware limits, payloads, keep-alive/TLS settings, upstream behavior, topology, and features.
- **Record:** hardware, OS, build settings, workload, sample count, percentiles, and errors.

### Current harness and baseline

`crates/wye-bench` implements the mock-first plan for the controlled part: a mock Anthropic upstream (unary and timestamped SSE), a load client, and the release `wye` binary as a separate process, compared directly against the mock. `scripts/check.sh` gates every commit on it ([development.md](development.md#commit-gate)).

Current baseline on the development laptop (i9-9880H, loopback, release build; median of 3 runs; `bench/baseline.json`):

| Metric | Plain HTTP/1.1 upstream | TLS + HTTP/2 upstream |
|---|---|---|
| Added unary latency p50 / p99 | 222 / 263 µs | 260 µs / (not gated) |
| Added time to first byte p50 | 223 µs | 299 µs |
| Added delay per SSE event p50 / p99 | 145 / 326 µs | 181 / 248 µs |
| CPU per streamed MB | 3.2 ms | 4.1 ms |
| Memory per idle stream | 28 KB | 5.6 KB (streams share connections) |
| Upstream release after client disconnect (max) | 157 µs | 99 µs |
| Bulk throughput through Wye | 1.26 GB/s (direct: 3.85) | 1.09 GB/s |
| 1 MiB upload: added latency p50, CPU per MB | 4.1 ms, 1.45 ms | 4.5 ms, 2.9 ms |
| 2,000 token-paced streams: delay per event p50 / p99 | (TLS only) | 1.8 / 4.9 ms |
| 2,000 token-paced streams: CPU per event, memory per stream | (TLS only) | 36 µs, 9.5 KB |

Run-to-run noise on this machine is about 10–15% for CPU per MB and larger for tail latency, and ±25% on the TLS path, so the gate reliably catches regressions of roughly 25% or ~100 µs, not smaller ones. The paced scenario saturates the whole laptop, so its latencies include machine load. **Heap allocations** (`alloc_` metrics, plain path, `alloc-stats` build; median of 3, spread under 1% except uploads):

| Metric | Value |
|---|---|
| Allocations per streamed SSE event | 0 |
| Allocations per stream setup | 23 |
| Allocations / allocated bytes per unary request | 20.6 / 21.6 KB |
| Allocations per authenticated request (cached credential) | 20.6 |
| Live heap per idle stream | 49 KB plain, 23 KB TLS |
| Live heap per idle stream after a 512 KB request | 77 KB plain, 48 KB TLS (150 KB TLS before the buffer cap) |
| 1 MiB upload: allocations / allocated bytes | 44 / 0.43 MB (28 / 1.27 MB before the buffer cap) |

Findings: the relay path allocates nothing per event. An idle stream holds about 49 KB of live heap while RSS grows only about 28 KB, because large connection buffers are allocated but not fully touched. The dominant production cost was different: after an agent-sized request, hyper's client-connection read buffer stayed grown for the stream's lifetime (150 KB per idle TLS stream after 512 KB); a 32 KiB cap brought it to 48 KB (see the techniques table). Per-request allocations fell from 33.5 to 20.6 by sharing the cached identity behind an `Arc` (an authenticated request now costs the same as an unauthenticated one) and serializing the usage log line from borrowed fields. Uploads allocated about 1.2× the body as receive buffers grew; with the 32 KiB buffer cap it is about 0.4×, in more, smaller allocations.

Gateway credentials (`auth_` metrics, plain path, per-request scenarios only): full verification costs about +88 µs of CPU per request (the Ed25519 check alone measured 55–62 µs in isolation); with the per-connection credential cache, authenticated requests on reused connections cost the same CPU as unauthenticated ones. Not covered yet: Linux and multi-machine load.

Model permissions (`models_` metrics: `[auth] models` plus a `models` claim, per-request and upload scenarios; one run under a load average of about 5; gated since, with a quiet-machine baseline): one more allocation per request (21.6 vs 20.6, the boxed policy scanner; a single held chunk allocates no queue), CPU per request 121 vs 113 µs with `[auth]` alone, upload CPU unchanged on the one-long-string body (1.56 vs 1.53 ms/MB), and +0.67 ms/MB on the Claude Code-shaped body (2.16 vs 1.49). That last cost is the check for a repeated top-level `model` key: it lexes the rest of the body, at the same per-byte cost as the diagnostics scanner. It applies only to requests whose credential or config restricts models.

OTLP log export (`otlp_` metrics, exporting to the mock; per-request scenarios, reported, not gated): CPU per request 116.5 vs 113 µs without export (within noise), added unary latency and SSE tails at low load in line with the other variants (event p99 151–158 µs). One run under a load average of 6 showed a 44 ms event p99 that two quiet reruns did not reproduce.

**Soak** (`wye-bench load --wye <alloc-stats build> --scenario paced --seconds 150`; TLS + HTTP/2, batches of 2,000 token-paced streams at 50 events/s, 34 batches, 6.8M events, zero failed streams, 34 µs CPU per event): live heap after each batch rose about 600 KB over the first ~20 batches (connection lanes and buffers warming) and then held at 49,363 KB through the end; resident memory levelled off near 56 MB (the allocator keeping freed pages). No growth per stream served. A multi-hour soak remains to be run: an overnight attempt (2026-10-18) was invalid because the laptop slept, waking for about 45 s every 30 minutes and pausing every stream; it needs a machine that stays awake (lid open on AC, or a server). `--scenario paced` now prints a line per batch as it runs, so an interrupted run keeps its series.

**Compressed responses** (2026-10-17, first real Claude Code session through Wye, subscription passthrough): every streamed response was recorded with `usage_status: unknown` and zero tokens, while streaming, session, request-class, and tool-duration fields were correct. Claude Code sends `accept-encoding: gzip, deflate, br, zstd` (captured with a local header-logging server), and `api.anthropic.com` answers with `content-encoding: br` when asked (checked with an unauthenticated request); Wye forwarded the header, so its scanner saw Brotli bytes. The mock never compresses, so no test caught it. Fix: Wye asks the upstream for `identity` ([architecture.md](architecture.md#forwarding-contract)); after it, a real request recorded the model, `complete` usage, and its cache write.

**Cost of uncompressed responses** (same day, offline, no provider calls): 353 real assistant responses (a 1-in-10 sample of local Claude Code transcripts: text, thinking, and tool-use blocks with their recorded usage) rebuilt as Anthropic SSE streams, with deltas of 8, 16, or 32 bytes of content, because the real delta size was not captured. Compression is flushed after every event, as a streaming server must; Anthropic's level and flush behavior are unknown, so the rows bracket them. Scratch tool, not in the repository (flate2, brotli, and zstd crates; i9-9880H, release build).

| Delta size | Identity per response | Per event | gzip -6 | Brotli q4 / q11 | zstd -3 | Brotli whole body (not streamable) |
|---|---|---|---|---|---|---|
| 8 B | 35.9 KB | 151 B | 4.0× | 5.7× / 5.8× | 5.8× | 12.0× |
| 16 B | 21.5 KB | 173 B | 3.5× | 4.5× / 4.6× | 4.6× | 7.7× |
| 32 B | 14.3 KB | 211 B | 3.1× | 3.6× / 3.7× | 3.7× | 5.4× |

Identity costs 3.5–6× the bytes from Anthropic to Wye, about 120–150 B more per event, or 10–30 KB per response. Little of that shows up as time. A response streams at the model's pace, about 20 events/s at 80 tokens/s, which needs about 3.5 KB/s (28 kbit/s) uncompressed. Sending 150 B more per event on a 10 Mbit/s link takes 0.12 ms, against a typical inter-token gap of 20 ms. In total, a 300-response session moves about 6 MB instead of 1.5 MB. The difference matters only on metered links. Request bodies, which are far larger in agent sessions, are unaffected. With Wye on loopback the client side costs nothing; on a network, Wye already sends uncompressed responses to the client, as before.

The rejected alternative, decompressing a copy in Wye, decoded Brotli q4 one frame per event at 6–11 µs per event. That adds 20–90% to Wye's whole relay cost of 12–35 µs per event (capacity and soak runs above). It would also need a decoder per stream whose window, up to several MB for Brotli and zstd, is attacker-controlled, and it would end the zero allocations per event on the streaming path. Asking for `identity` remains the choice.

**Capacity per core** (2026-10-16, `wye-bench load --wye <release> --scenario capacity`; Wye with `TOKIO_WORKER_THREADS=1`, TLS + HTTP/2, batches of token-paced streams at 50 events/s of 100 bytes, each run first directly against the mock and then through Wye; two runs, load average about 2):

| Streams | Events/s | Wye cores | CPU µs/event | Direct p99 | Added p99 |
|---|---|---|---|---|---|
| 500 | 11,900 | 0.24 | 20 | 0.9–1.1 ms | 4.1–4.4 ms |
| 1,000 | 23,900 | 0.37–0.39 | 16 | 1.5–2.2 ms | 6.0–6.4 ms |
| 1,500 | 35,800–35,900 | 0.46–0.48 | 13 | 2.6–3.0 ms | 8.3–8.9 ms |
| 2,000 | 47,900 | 0.57–0.58 | 12 | 3.2–3.3 ms | 12.1–13.0 ms |
| 3,000 | 70,500–71,300 | 0.71–0.73 | 10 | 4.1–4.3 ms | 67–79 ms |
| 4,000 | 88,400–91,000 | 0.74–0.76 | 8 | 5.2–5.3 ms | 219–258 ms |

Sustainable (added p99 at most 10 ms, no failures): 1,500 streams, about 36,000 events/s on one worker thread. The limit is queueing on the single thread, not CPU: average utilization is below one core because each batch includes ramp-up and tail, while the peak saturates. CPU per event falls with load (events batch into fewer wakeups and writes) and is about a third of the 35 µs measured with the default multi-thread runtime at 2,000 streams, which spends the difference on cross-thread wakeups; whether fewer worker threads serve more streams per core in production needs a multi-core capacity run (research, not a default change).

**More worker threads** (same day, `WYE_BENCH_GATEWAY_THREADS=2` and `4`): with 2 threads, 3,000 streams (71,500 events/s) stayed within 10 ms added p99 (8.2 ms; Wye at about 1.0 core, 14 µs per event), twice the one-thread figure; 4 threads reached only 2,000 streams (3.7 ms; 11.9 ms at 3,000) at 21–38 µs per event. Both collapsed at 4,000 streams (0.5–1 s added p99) with Wye below its cores. The cause is the laptop, not Wye: sampled with `ps`, the bench process (client and TLS mock in one process) used 4–5 cores at 3,000 streams, on an 8-core machine also running Wye, and one repeat showed a 100 ms direct p99 at that step. Multi-thread capacity therefore needs a separate load machine; on one laptop, the one- and two-thread rows are the trustworthy ones, and they scale linearly. CPU per event rising with worker threads (12–20 µs at 1, 14–27 at 2, 21–38 at 4) supports measuring a smaller default worker count on a real host.

**Profiles.** On the plain path, Wye's CPU is almost all system calls (`writev` and `recvfrom` per frame, connection setup); its own code is a small share. On the TLS path, profiles were first dominated by lock contention in `h2`, which guards all streams of one connection with one mutex; spreading streams over connections ([architecture.md](architecture.md#upstream-connections)) cut CPU per MB from 12.3 to 4.1 ms and unary added latency from 1.5 to 0.3 ms. Remaining CPU gaps come from relaying each frame as it arrives and from handing each chunk between the upstream and downstream connection tasks.

### Against KrakenD

A reference comparison with a mature general gateway. `wye-bench run ... --krakend <path>` runs every scenario through KrakenD 3.0.1 CE as well, in its documented pass-through mode (`no-op` encoding, all headers and query strings, one backend) and its best case (no access log, ERROR logging), while Wye keeps its per-request usage log.

**KrakenD CE cannot stream SSE.** In `no-op` mode it buffers the whole response: an event sent at 0 s arrived when the stream ended, 3 s later. `"sse"` encoding is Enterprise-only; CE answers it with 500. For Claude Code that means no tokens appear until the reply is complete. The idle-memory and cancel scenarios, which wait for the first event of a silent stream, therefore run only for Wye.

Single run on the development laptop, all 16 cores, Wye at `35a0584` (2026-10-15; the first comparison, a median of 3 at an earlier commit, showed the same picture):

| Metric | Wye | KrakenD |
|---|---|---|
| Added unary latency p50 / p99, plain | 213 / 184 µs | 445 / 1,314 µs |
| Added unary latency p50 / p99, TLS | 260 / 629 µs | 371 / 2,301 µs |
| CPU per request, TLS | 161 µs | 252 µs |
| Added delay per SSE event p50, TLS | 0.15 ms | 34.4 ms (buffered) |
| Added time to first byte p50, TLS | 0.45 ms | 72 ms (buffered) |
| RSS after load, TLS | 53 MB | 302 MB |
| CPU per streamed MB, TLS | 4.4 ms | 1.5 ms |
| Bulk throughput, TLS | 1.0 GB/s | 2.3 GB/s |

With equal CPU budgets (`TOKIO_WORKER_THREADS` / `GOMAXPROCS`), Wye still wins streaming and memory, and unary latency at 2 threads (TLS p50 828 vs 1,224 µs); at 4 threads TLS unary is a tie (715 vs 679 µs). KrakenD keeps a 1.5–2× lead on bulk CPU per MB (4 threads: 1.43 vs 2.29 ms plain, 3.16 ms TLS; 2 threads: 1.02 vs 2.1 ms).

**Relevance.** Bulk throughput, the one area KrakenD leads, matters least for LLM traffic: streams are token-paced (KB/s), and Wye's added 0.2–0.3 ms is under 0.1% of a typical model response. What matters to users is streaming without buffering, concurrency without queuing, memory per idle stream, cancellation, and p99 under load; the [realistic-workload benchmark](roadmap.md#realistic-workload-benchmark) measures those workloads.

**Realistic workloads** (`upload_*`, `agent_upload_*`, `tls_paced_*`; the same single run, so numbers differ slightly from the gated baseline above):

| Workload | Metric | Wye | KrakenD |
|---|---|---|---|
| 1 MiB uploads, concurrency 16, TLS | added latency p50 / p99 | 3.7 / 7.6 ms | 1.9 / 2.5 ms |
| | CPU per uploaded MB | 3.1 ms | 2.0 ms |
| 1 MiB Claude Code-shaped uploads, plain | added latency p50, CPU per MB | 2.8 ms, 1.49 ms | 3.7 ms, 1.79 ms |
| 2,000 streams × 50 events/s × 100 B, TLS | added delay per event p50 / p99 | 1.5 / 5.4 ms | 295 / 572 ms (buffered) |
| | CPU per event | 35 µs | 55 µs |
| | memory per active stream | 7.9 KB | 95 KB |

Change since the first comparison: KrakenD now leads TLS uploads (2.0 vs 3.1 ms CPU per MB, 1.9 vs 3.7 ms added p50), where the first run tied; on plain HTTP uploads the two are level (1.45 vs 1.49 ms/MB) and Wye leads the Claude Code-shaped body. The 32 KiB client-connection buffer cap adopted since explains little of it: an A/B (2 rounds, `scripts/ab.sh`) without the cap measured 2.46 vs 2.62 ms/MB on TLS uploads (−6%) and 1.32 vs 1.42 on plain ones (−7%), not enough to reach KrakenD's 2.0, and the cap stays for its memory saving. The likelier cost is the same as for bulk transfer below: every upload chunk crosses from the client connection task to the upstream connection task. Wye's gated TLS upload cost is 2.6–2.9 ms/MB.

**TLS upload investigation** (2026-10-15, `wye-bench load`, 8 s of sustained 1 MiB uploads at concurrency 16, CPU per MB):

- *Unequal protocols.* KrakenD talks HTTP/1.1 to the TLS mock (Go does not negotiate HTTP/2 with a custom TLS config); Wye negotiates HTTP/2. Like for like, Wye over HTTP/1.1 measured 2.46 ms against KrakenD's 2.15–2.22 (about 10%); Wye's HTTP/2 costs another ~9% (2.69 ms).
- *Where HTTP/2 spends it* (`sample`): of busy samples, about 45% are `writev` from rustls, 15% `recvfrom`, 16% AES-GCM, 10% waits on h2's connection mutex. h2 buffers one DATA frame at a time and flushes per frame, so each frame costs an encryption and a syscall; with the mock's 16 KiB maximum frame size, larger frames cut the cost sharply (64 KiB: 2.59 ms; 256 KiB: 2.29 ms).
- *The mock does not look like Anthropic.* `api.anthropic.com` advertises (HTTP/2 SETTINGS, read with a preface and no request) `MAX_FRAME_SIZE` 16,777,215, `INITIAL_WINDOW_SIZE` 65,536, `MAX_CONCURRENT_STREAMS` 100, and a ~2 GiB connection window; the mock uses hyper's defaults (16 KiB frames, 1 MiB windows). With Anthropic's settings in the mock, Wye measured 2.65 ms: the small per-stream window replaces the small frames as the limit.
- *Possible latency cost, unmeasured.* Under HTTP/2 flow control a stream can have at most its window in flight, so an upload proceeds at most at window ÷ RTT: about 2 MB/s at 64 KiB and 30 ms, roughly 0.5 s for a 1 MiB Claude Code context, unless the server enlarges the stream window after the request starts. HTTP/1.1 is bounded only by TCP. This needs a measurement against the real API: `scripts/upload-latency.sh` times a ~500 KB `count_tokens` request (free) over HTTP/1.1 and HTTP/2, directly and optionally through Wye, with an API key. `upstream.http_version = "1.1"` makes Wye use HTTP/1.1 to a TLS upstream (each concurrent request then needs its own upstream connection); the default stays HTTP/2 when offered until the measurement says otherwise.

The paced scenario loads the whole laptop (about 100k events/s, with mock, load client, and gateway sharing 16 threads), so its latencies include machine saturation; CPU per event and memory per stream are the precise signals. Stream starts are ramped over 2 s because macOS caps the listen backlog at 128; it runs only on the TLS + HTTP/2 path because over HTTP/1.1 every stream opens its own connections and repeated runs exhaust ephemeral ports. Unmeasured so far: Linux and multi-machine load; LiteLLM is the streaming-capable comparison ([below](#against-litellm)).

### Against LiteLLM

`wye-bench run ... --litellm <path>` runs every scenario through LiteLLM 1.105 (Python 3.14), in its leanest proxy configuration: the byte-level pass-through endpoint (`pass_through_endpoints`, `forward_headers`, mounted at `/pt/v1/messages` because LiteLLM's own `/v1/messages` parses and translates requests), no database and therefore no spend logging, one worker per core (16), upstream CA through `SSL_VERIFY`. It streams SSE, so every scenario runs. Its CPU and memory are summed over the whole process tree.

Single run on the development laptop, Wye at `35a0584` (2026-10-15); zero failed requests or streams on either gateway:

| Metric | Path | Wye | LiteLLM |
|---|---|---|---|
| Added unary latency p50 / p99 | TLS | 0.26 / 0.63 ms | 44 / 141 ms |
| CPU per request | TLS | 161 µs | 19.8 ms |
| Added time to first byte p50 | TLS | 0.45 ms | 14 ms |
| Added delay per SSE event p50 / p99 | TLS | 0.15 / 0.30 ms | 0.43 / 18 ms |
| 2,000 paced streams: delay per event p50 / p99 | TLS | 1.5 / 5.4 ms | 34 ms / 0.61 s |
| 2,000 paced streams: CPU per event | TLS | 35 µs | 312 µs |
| 1 MiB upload: added latency p50 | TLS | 3.7 ms | 54 ms |
| 1 MiB upload: CPU per MB | TLS | 3.1 ms | 52 ms |
| 1 MiB Claude Code-shaped upload: CPU per MB | plain | 1.49 ms | 190 ms |
| Memory per idle stream | TLS | 6.8 KB | 41 KB |
| Process memory at idle | TLS | 27 MB | 8.7 GB |
| Bulk throughput | TLS | 1.02 GB/s | 0.26 GB/s |

Caveats: all processes share one 16-thread laptop, which penalizes LiteLLM's 16 workers under the load generator's competition; LiteLLM's regular API path (request parsing, translation, logging) would cost more than this pass-through. Plain-path memory per idle stream was negative for LiteLLM (noise on an 8.6 GB total) and is not a result.

Benchmark fixes this comparison required, each a real flaw: mock payloads were not valid JSON (LiteLLM parses them; Wye does not, so the flaw was invisible before); unary and upload scenarios counted fast error responses as successes (now `*_failed_requests`, gated at zero); the test CA lacked key usage and the server certificate an Authority Key Identifier and a distinct subject, all required by Python 3.13+ strict X.509 verification but not by rustls; forked gateway workers outlived their pass (gateways now run in their own process group, killed on drop).

Why bulk CPU differs, and what did not help:

- KrakenD writes each buffered response in a few large writes; Wye relays each upstream frame as it arrives, which is what streaming requires.
- Wye's upstream connection runs in a hyper task separate from the downstream connection task, so every chunk crosses tasks and often threads. With fewer worker threads Wye's CPU per MB drops sharply (16 threads: 3.6 ms; 1 thread: 1.7 ms) while latency under load rises; idle Tokio workers also spin looking for work.
- Merging already-available upstream frames before writing (adaptive coalescing): no gain (+1% to +5% CPU per MB); hyper already batches ready frames into one `writev`. Rejected.
- Remaining options: run the upstream connection inside the downstream task (a custom HTTP/1.1 and HTTP/2 client driver; large change), size the runtime to the CPU budget by default, and fat LTO (−6% to −8% CPU earlier, inconclusive on p99).

Microbenchmarks alone cannot establish proxy performance. Separate controlled overhead measurements from variable real-provider behavior. A gateway with more middleware enabled is not a fair baseline unless the difference is documented. Rust-versus-Go comparisons require measured results.

## Order of work

1. Establish forwarding correctness and a direct-access baseline.
2. Measure overhead and bounded resource behavior of the streaming proxy.
3. Test cancellation locally, then investigate provider-side effects.
4. Evaluate routing only after its implementation and comparison criteria are defined.

## Related work

| Work | Core idea | Fit with no shared state |
|---|---|---|
| *The Tail at Scale* | Hedged requests, tail mitigation | Request-local; risks duplicate billing |
| Netflix `concurrency-limits` | RTT-derived adaptive admission | Needs ephemeral cross-request local state |
| Maglev / rendezvous hashing | Stable selection under membership change | Deterministic; useful only if affinity matters |
| HTTP/2 flow control | Per-stream and connection credit | Within a connection's lifetime |
| FrugalGPT / RouteLLM | Offline quality-aware model selection | If the policy is static at runtime |
| Macaroons / Biscuit | Offline-verifiable attenuated authorization | Authorization, not cumulative usage |
| Privacy Pass / offline e-cash | Redemption and double-spend limits | Shows need for spent-token state or trusted hardware |
| RLN / nullifier rate limiting | Anonymous cryptographic rate limiting | Verification stateless; replay tracking is not |
| Bounded Counter CRDT / escrow | Distributed numeric invariants | Requires coordination; rejected for core ([policy.md](policy.md#rejected-approaches)) |
| [TraceLab](https://arxiv.org/abs/2606.30560) (2026) | Public trace of ~4,300 Claude Code and Codex sessions (~350k LLM steps): long loops, long contexts with short outputs, heavy-tailed tool calls, high but imperfect prefix-cache hit rates | Workload source for the benchmark; evidence for cache-miss diagnostics |
| [CacheWise](https://huggingface.co/papers/2606.16824.md) (2026), [Agentic Coding in the Wild](https://www.microsoft.com/en-us/research/publication/agentic-coding-in-the-wild-characterizing-github-copilot-at-production-scale/) (Microsoft/UIUC, 2026) | Coding-agent traces: large repeated prefixes; agent loops coupled nearly 1:1 with tool calls (761M Copilot calls) | Confirms uploads and per-event cost, not bulk throughput, as the metrics that matter |
| [CacheRoute](https://arxiv.org/pdf/2608.19677) (Meta, 2026), [KV-cache-aware load balancing](https://www.arxiv.org/pdf/2601.18999) (2026), llm-d, Ray Serve prefix routing | Prefix-affinity routing among model servers; served cache hits 64% → 93%, with a warning that affinity can cost capacity | Applies to routing among provider accounts; motivates a stateless load bound on `cache_affine` |
| [R2-Router](https://arxiv.org/abs/2602.02823) (ICML 2026), [When Routing Collapses](https://arxiv.org/abs/2602.03478) (2026), [ContextualRouter](https://aclanthology.org/2026.eacl-srw.22) (EACL 2026) | Route by model and output-length budget together; routers drift toward the strongest model; kNN baselines match complex routers | Offline-fitted, simple routing tables fit static config; length budgets match the allowlisted `max_tokens` rewrite |
| [IETF Attenuating Agent Tokens](https://datatracker.ietf.org/doc/html/draft-niyikiza-oauth-attenuating-agent-tokens-00) (draft, 2026), WIMSE agent delegation chains (draft), DeepMind Delegation Capability Tokens ([survey](https://arxiv.org/pdf/2603.24775)) | JWT-based offline attenuation for task-scoped agent delegation, with depth and lifetime limits | Standards-track format for delegable credentials; aligns with Wye's EdDSA JWTs. Drafts may still change |

Summaries come from abstracts and secondary sources; check figures against the papers before citing them.

## Experiments

Each experiment names its status; feature-shaped experiments moved to [feature directions](#feature-directions).

- **A. Streaming backpressure at scale.** Thousands of mixed-speed SSE clients without memory growth or starvation. *Partly done:* 2,000 token-paced streams and 1,000 idle streams are gated, and a slow-reader test proves upstream backpressure; 10k streams and mixed fast/slow consumers remain (M5).
- **B. Cost-constrained tail latency.** Can routing or request-local hedging improve p99 TTFT without duplicate charges? Compare fixed route, cheapest eligible, bounded fallback, and connect-only hedging. *Open;* needs multiple candidates (M4).
- **C. Cost-aware routing.** Evaluate cost per successful task at a quality threshold, not token price. *Became* [agent-aware routing](#1-agent-aware-routing).
- **D. Capability delegation.** *Became* [delegable credentials](#3-delegable-credentials-for-agent-swarms).
- **E. Native Tokio versus Go.** Feature-equivalent proxies under long-lived streaming load. *Partly done:* KrakenD (Go) measured; KrakenD CE cannot stream SSE, so the comparison is bulk-only there.
- **F. Edge versus native.** Same policy under native Tokio and a WASM adapter. *Open* (M6).

## Feature directions

Research-backed functionality that fits the constraints: request-local decisions, no shared state, config in Git, analysis delegated to the telemetry backend. Wye's direction is native passthrough with everything that is not application logic in config ([product.md](product.md#principles)); understanding agent traffic is its distinctive part, built by directions 1 and 2 and the [agent-native directions](#agent-native-directions) below. Model aliases and upgrade checks are designed in [architecture.md](architecture.md#model-aliases-and-upgrade-checks-planned). Implemented: cache-miss diagnostics (2) and clean stream endings (in 7); the others are not.

### 1. Agent-aware routing

- **Basis:** LLM cascades and offline-trained routers (FrugalGPT, RouteLLM; 2026: R2-Router, EquiRouter, ContextualRouter), applied to the gateway hint headers Claude Code documents for routers: `x-claude-code-request-class` (`main`, `subagent`, `workflow`, `compaction`, `auxiliary`), `x-claude-code-agent-type`, compaction markers.
- **Function:** policy such as "auxiliary requests use the Haiku model" or "Explore subagents use Sonnet", applied as the allowlisted `model` rewrite, within Claude models only. Weights are fitted offline from usage telemetry and committed as fixed config (closed loop through Git: telemetry → fit → pull request → policy tests → deploy).
- **Fit:** a pure function of headers and config.
- **Design notes from 2026 work:** simple tables over request features (kNN-like) are competitive, so the fitted policy can stay a readable config table; choosing an output-length budget with the model saves more than model choice alone, and maps onto the allowlisted `max_tokens` rewrite; telemetry should watch for routing collapse (a growing share sent to the strongest model).
- **Limits:** a custom base URL receives hint headers only when developers set `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` (deliverable through managed settings); without them, routing falls back to the requested model. Quality impact must be measured per request class before a default ships.

### 2. Cache-miss diagnostics

- **Basis:** prompt-cache pricing (cached input costs about a tenth of normal input) and content fingerprints.
- **Function:** for each request, export a hash per cacheable block (system, tools, each message prefix) next to the reported cache usage. The telemetry backend compares consecutive requests of a session and names the cause of each miss: tools reordered, a timestamp in the system prompt, a compaction.
- **Fit:** Wye hashes within one request; the comparison across requests happens in the user's backend, never in Wye.
- **Evidence:** coding-agent traces (TraceLab, CacheWise) show high but imperfect prefix-cache hit rates, with misses concentrated around user-initiated turns and compaction, so misses have explainable causes.
- **Pairs with** cache-affine routing ([architecture.md](architecture.md#cache-affine-routing)), which keeps a session on one provider account from any replica; prefix-affinity serving research (CacheRoute) shows large hit-rate gains and warns that affinity needs a load bound.
- **Implemented** (`gateway-core/src/prompt.rs`, enabled by `[cache_diagnostics]`; fields in [operations.md](operations.md#telemetry)):
  - *Recorded per request:* keyed hashes of the whole `tools` and `system` sections with their element counts; the number of messages; each `cache_control` breakpoint (the first four with position: section, element, and content block for messages, plus the hash of its section through the marked block; all of them counted); and the hashes of the last eight message prefixes (`messages[0..=i]`).
  - *One hash per section, not one cumulative hash:* the fields of a body may arrive in any order, so hashing in cache order (`tools` → `system` → `messages`) would need buffering. The backend compares the sections in that order instead, which locates the same change.
  - *Marker and formatting excluded:* `cache_control` members (with their separating comma) and whitespace outside strings are left out. Claude Code moves the marker to the newest message every turn; hashed, it would make every earlier prefix look changed.
  - *Explaining a miss* happens in the telemetry backend: for consecutive requests of one session (`x-claude-code-session-id`), compare `tools`, then `system`, then the message prefix at the previous request's last message index (a turn usually appends two messages, so it is among the eight kept). The first mismatch names the cause: tools added or reordered, a changed system prompt, or an edited or compacted history.
  - *Streaming scanner:* follows strings, escapes, and nesting with constant memory, independent of chunk boundaries. Values whose structure does not matter (tool schemas, tool inputs, marker values) are lexed only for nesting and strings. A body it cannot follow (malformed, truncated, a repeated section, an escaped key where the name matters) is recorded as `unknown` and is forwarded unchanged like any other.
  - *Hash and key:* xxh3 (64-bit) inside the process; every exported hash is HMAC-SHA256 under a key all replicas share, truncated to 64 bits, so hashes compare across replicas but cannot be matched against guessed prompts. The key is required whenever diagnostics are on (decided: no unkeyed fallback). The log writer task computes the tags, off the request path.
  - *Not covered:* markers nested deeper than message content blocks, and the top-level automatic-caching marker. Raw bytes are hashed, not a canonical form, so hashes compare between bodies one client serialized.
  - *Verification:* a property test renders random bodies with random whitespace and markers, feeds them in random chunkings, and compares against xxh3 of the canonical rendering; integration tests cover the log record.
- **Cost** (i9-9880H, 3-run medians from the commit gate, load average about 2):

  | Measurement | Without | With diagnostics |
  |---|---|---|
  | Allocations per unary request | 20.6 (21.6 KB) | 22.6 (22.8 KB): scanner box and result slot |
  | CPU per unary request | 111.5 µs | 107.5 µs (no difference beyond noise) |
  | Upload CPU, 1 MiB body of one long string (best case) | 1.79 ms/MB | 1.75 ms/MB |
  | Upload CPU, 1 MiB Claude Code-shaped body (`agent_upload`) | 1.86 ms/MB | 2.75 ms/MB (+48%) |
  | Scanner alone on that body (release, best of 7) | | 0.61–0.69 ms/MB |
  | Allocations per agent-shaped upload | 33 (266 KB) | 58 (604 KB) |

  Against the design target: two allocations per request instead of none (sharing the result between the request body and the usage record needs one heap slot, and the scanner is boxed so requests without diagnostics stay small); under 10% extra upload CPU holds for long-string bodies but not for structure-dense ones. There the scanner runs within about 15% of a naive byte-at-a-time lexer that tracks only nesting and strings (0.54 ms/MB on the same body). So the remaining cost is in lexing itself, and the next step is a SIMD structure classifier in the style of simdjson, which needs `unsafe` intrinsics with a proof and tests. End-to-end CPU grows by more than the scanner alone: slower body consumption also makes hyper allocate more read buffers (12–24 more allocations per agent-shaped upload across runs, all transient). Rejected after A/B: an 8-bytes-at-a-time (SWAR) search for short string ends (no gain over a byte loop); kept: forced inlining of the lexer's hot functions (about 20% faster).
- **SIMD prototype, not built** (throwaway example on the same body, best of 7): simdjson-style stage 1 (classify 64-byte blocks, find escaped quotes and string spans with bit arithmetic, visit brackets, separators, and quotes outside strings) took 0.17–0.21 ms/MB with AVX2 and PCLMUL and 0.24 ms/MB with SSE2 alone; classification itself was 0.07 ms/MB and xxh3 of the body 0.05 ms/MB. Visiting the ~50,000 structural positions per MB dominates, and the real state machine does more at each than the prototype's depth counter, so a full SIMD scanner would land near 0.35–0.4 ms/MB: about 1.7× faster than the current scanner, still above the 10% target, for the first `unsafe` code on the request path and one code path per architecture. Chosen instead: **per-session sampling** (`sample_percent`). Whether a session is diagnosed is a hash of its `x-claude-code-session-id`, so every replica decides alike, sampled sessions keep the complete request sequence a miss explanation needs, and fleet CPU falls in proportion to the rate.

### 3. Delegable credentials for agent swarms

- **Basis:** Macaroons and Biscuit: a token holder mints a narrower token offline. The 2026 IETF draft "Attenuating Agent Tokens" specifies this as a JWT format with delegation-depth and lifetime limits, motivated by prompt injection and compromised subagents misusing over-broad tokens; implementing it rather than a private format makes Wye interoperable with other agent tooling, once the draft stabilizes.
- **Function:** team → developer → CI job → subagent, each narrowing models, output caps, expiry, or scope without contacting an issuer.
- **Fit:** verification is a pure computation over signatures.
- **Limits:** caps each request, never cumulative spend (that stays with [provider-held budgets](policy.md#provider-held-budgets)); replay inside the validity window remains possible ([policy.md](policy.md#why-signed-balances-fail)).

### 4. Signed usage receipts

- **Basis:** tamper-evident logs and transparency-log Merkle batching.
- **Function:** a signed receipt per request (request hash, usage, cost bound, config version, credential subject) exported through OTLP, optionally Merkle-chained so deletions are detectable. Chargeback and audit that finance can trust, without a billing database.
- **Fit:** needs only a signing key; storage is the user's telemetry backend.
- **Design (proposal, not built):**
  - *What a receipt attests:* "this replica forwarded this request for this credential subject, and the provider reported this usage for it". It does not prove what the provider billed; finance reconciles through the provider's own request id.
  - *Contents:* receipt version, key id, replica instance id (random per process start) and sequence number, end time, credential subject, Claude Code session id, route, model, status, outcome, the provider-reported token counts and usage status, the provider's `request-id` response header, config version once it exists, and `prev`, the SHA-256 of the previous receipt from the same replica instance.
  - *Encoding:* a compact JWS (`alg` pinned to `EdDSA`, the same strict format as gateway credentials) over a canonical JSON payload, emitted as the `receipt` field of the usage log line and the `wye.receipt` attribute of the OTLP log record.
  - *Tamper evidence without shared state:* each replica instance keeps its own hash chain (sequence numbers plus `prev`), so a deleted or altered receipt breaks its chain or leaves a gap. Chains start fresh at each process start and never coordinate across replicas; the chain state is one hash and one counter in the writer task. Periodic chain heads (sequence and hash) can be exported as a small OTLP record for external anchoring.
  - *Verification:* `wye receipt verify --key <public key> <receipts.jsonl>` checks signatures, chain links, and gaps per instance, offline.
  - *Configuration:* `[receipts] signing_key_file` (an Ed25519 key from `wye token keygen`, kept apart from credential-issuing keys) and `key_id`; the public key goes to the auditors.
  - *Not included:* a hash of the request body. Measured on the development laptop: SHA-256 of a 1 MiB body costs 2.16 ms (no SHA extensions on this CPU), more than Wye's whole upload cost of 1.45 ms/MB; the provider's `request-id` ties a receipt to a request instead. A keyed prompt hash from [cache-miss diagnostics](#2-cache-miss-diagnostics) can be included when both are on.
  - *Cost:* one Ed25519 signature per request, measured at 13.7 µs for a 400-byte payload, plus a SHA-256 of the receipt (about 1 µs), in the writer task, off the request path. At 1,000 requests per second per replica that is about 1.5% of one core.

### 5. Queryable and provable policy

- **Basis:** automated reasoning over access policies (AWS Zelkova for IAM, using SMT).
- **Function:** beyond `wye explain` for one request: "who can reach Opus?", "can any credential exceed $0.05 per request?", and a CI check that fails when a config change widens access.
- **Fit:** static config and routing as a pure function are what make the analysis possible.

### 6. Provable cost bound

Planned for M2 ([policy.md](policy.md#provable-cost-ceiling)): a guaranteed upper bound per request without a tokenizer. Research: tighten it by subtracting non-billable framing (JSON syntax, separately priced image bytes) in one pass, and measure bound ÷ actual cost on real sessions.

### 7. Reliability

- **Connect-only hedging** (*The Tail at Scale*): race a second connection attempt, never a second request, so tail latency drops without duplicate billing.
- **Deterministic simulation testing** (FoundationDB style): Wye, mock, and clients in a simulated network (turmoil or madsim) with seeded loss, slow readers, mid-stream drops, and shutdown.
- **Clean stream endings** (**implemented**): when the upstream stalls past the idle timeout or its body fails at an SSE event boundary, Wye sends an Anthropic `error` event (`overloaded_error`) and closes cleanly, so Claude Code sees a retryable overload instead of a dropped connection. Mid-event failures still drop the stream. The per-frame cost is one check of the frame's last bytes.
- **Proof-of-possession credentials** without a replay cache: bind tokens to a client key; replay inside a short window remains possible.

### Agent-native directions

Researched 2026-10-15 from Anthropic's gateway documents ([compatibility guide](https://code.claude.com/docs/en/llm-gateway-protocol), [rollout](https://code.claude.com/docs/en/llm-gateway-rollout), [connect](https://code.claude.com/docs/en/llm-gateway-connect)), the prompt-caching study for long agent tasks ([arXiv:2601.06007](https://arxiv.org/abs/2601.06007): 41–80% cost savings from cache placement; dynamic content mid-prompt breaks caches), and practitioner reports on agent cost runaways and cache regressions (vendor-authored; signals, not data). Built as roadmap stages A1–A4 ([roadmap.md](roadmap.md#m1b-agent-aware-visibility)); behavior is specified in [operations.md](operations.md#telemetry). This section keeps what was learned.

**What a gateway sees without reading bodies.** Every Claude Code request carries `x-claude-code-session-id`, subagent requests `x-claude-code-agent-id` and `x-claude-code-parent-agent-id`. With `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` (deliverable through managed settings) requests also carry the request class, agent type, compaction markers, a prompt id shared by every request one user prompt causes, and the run times of the tools between turns. None carries prompt text.

| Direction | Stage | Result |
|---|---|---|
| Retry hygiene: Wye's 529s carry `retry-after: 1` and `x-should-retry: true` | A1 | Built. Claude Code stops retrying above 60 s |
| Model picker from policy (`GET /v1/models`) | A1 | Built. A pure function of config and credential |
| Spend by work type: agent headers in records and `wye.tokens` | A2 | Built. Capturing the seven headers costs 0.32 µs per request when all are present, 0.04 µs when none are; validating them in the writer task 0.11 µs (i9-9880H) |
| Expected versus unexpected cache misses | A2 | Built as `cache_miss`. A session's first request also reads `unexplained`: one request cannot know it is first, so `wye report` and the backend exclude those by session |
| A session as a trace | A3 | Built flat: trace id from the session id, tool durations as spans. Synthetic prompt and subagent parent spans were dropped: every request would emit them (duplicate span ids) or exactly one would, and no request knows it is first. Checked in Jaeger 1.62 across two replicas |
| `wye report` and tool calls from responses | A4 | Built. Parsing `content_block_start` borrowed costs 0.22–0.33 µs per content block and allocates nothing |
| Report-only secret detection | A5 | Open: needs a multi-pattern matcher (likely a dependency) and a measured, session-sampled body scan. Report-only, because blocking would brick a session after one file read and redaction would break prompt caching |

**Deliberately not proposed:** per-session loop detection or spend caps (cross-request state; alert on the OTLP metrics instead), response caching (shared state), prompt rewriting for cache efficiency (beyond the allowlist, breaks preserved thinking), and blocking tool calls mid-stream (agent control is a non-goal).

### Excluded: needs shared state

Semantic or response caching (a cache store), global rate limits and redemption-based tokens such as Privacy Pass (shared counters or spent-token state), and adaptive per-node concurrency (cross-request history). See [policy.md](policy.md#rejected-approaches).

## Performance techniques

The workload: few long-lived requests, tiny SSE events at token pace, time to first token more important than throughput, and any duplicated request possibly billed twice. Each technique is a hypothesis until `wye-bench` measures it.

| Technique | Idea | Status |
|---|---|---|
| HTTP/2 to the provider | Multiplex client streams over few upstream connections | **In effect.** Negotiated by ALPN (verified against `api.anthropic.com`); benchmarked on the TLS path |
| Spread streams over connections | 64 lanes, least-loaded selection, instead of one shared HTTP/2 connection per host | **Adopted.** Fixed silent queuing beyond the provider's stream limit (a 4-stream mock now serves 10 streams), and avoids the single `h2` connection mutex: TLS unary added latency −80%, CPU per MB −66%, throughput 4×; process RSS after load 10 → 30 MB |
| Per-connection credential cache | Verify a credential once per connection; later requests with the same bytes compare in constant time and recheck expiry | **Adopted.** Two A/B runs: CPU per authenticated request −38% and −44% (195 → 121, 199 → 112 µs, equal to unauthenticated), added latency p50 −16% and −34%; other metrics unchanged. Memory: one verified token (~0.3 KB) per connection |
| Shared cached identity | Keep the cached credential's identity behind an `Arc` instead of cloning its strings per request | **Adopted:** with the borrowed log line, allocations per authenticated request 37.5 → 20.6, equal to unauthenticated |
| Cap the client-connection buffer | `max_buf_size` 32 KiB instead of hyper's ~400 KiB, so a large request body does not leave a grown buffer for the stream's lifetime | **Adopted (approved tradeoff).** Idle stream after a 512 KB request: TLS 150 → 48 KB (−68%), plain 183 → 77 KB; allocated bytes per 1 MiB upload 1.27 MB → 0.43 MB. TLS upload cost unchanged in A/B (CPU −0.7%, p50 −5%); plain-HTTP uploads +7% CPU, +11% p50, and 28 → 44 allocations per upload (smaller reads). 16 KiB saved more memory but cost +50% plain upload CPU. Headers above 32 KiB get 431 |
| Small request-body type | hyper's client sends requests through a channel whose block holds 32 of them inline, allocated per upstream connection, so each byte of the body type costs 32 bytes per connection (per stream on the plain path) | **Adopted:** permission state is boxed behind one pointer; holding it inline (88 bytes) cost +2.8 KB per idle plain stream and was blocked by the gate |
| Smaller write buffers | hyper allocates an 8 KiB header write buffer per HTTP/1.1 connection, not configurable | Not possible without changing hyper; part of the 23 KB per idle TLS stream |
| Fast cancellation as a cost metric | Release the upstream immediately when a client disconnects, even while it is silent | **Measured and gated:** release within ~80 µs p50, under 0.2 ms max. Provider-side effect (does billing stop?) unverified |
| Zero-copy usage extraction | SIMD `memchr` line scanning; JSON only for usage events | **Implemented.** Byte-by-byte scanning cost +49% CPU per MB and was blocked by the gate; `memchr` costs +5.8% |
| Pre-warmed connections | Keep warm TLS connections per provider to cut cold-start TTFB | Planned; needs a mock with configurable connect delay |
| Adaptive write coalescing | Merge already-available frames into one write | **Rejected (measured):** no CPU gain; hyper already batches ready frames into one `writev` |
| Lazy idle deadline | Timestamp per frame instead of resetting the idle timer | **Rejected (measured):** no gain |
| Fat LTO + one codegen unit | Whole-program optimization | **Not adopted:** CPU per MB −6% to −8%, binary 5.8 → 3.7 MB, but per-event p99 worse in both A/B runs and builds 2× slower |
| One HTTP/2 lock acquisition per frame | Record completion inside hyper's own `is_end_stream()` call | **Candidate:** strictly less work, but A/B inconclusive (−5%, +12%) within TLS noise |
| Fewer runtime threads | Size Tokio workers to the CPU budget | **Deployment knob** (`TOKIO_WORKER_THREADS`): CPU per MB 3.6 → 1.7 ms from 16 to 1 thread, but latency under load rises |
| mimalloc | Replace the system allocator | **Rejected (measured):** idle memory per stream +9%, RSS +12%, CPU per MB +4% |
| kTLS + splice | Kernel TLS, socket-to-socket without user-space copies | Research only: Linux-only, high-throughput only, incompatible with usage extraction |
| Thread-per-core runtime | monoio or glommio instead of Tokio work stealing | Research only, if CPU per MB becomes the limit |
| Transparent stream repair | Resume a dropped stream with assistant prefill | **Rejected:** changes semantics and billing, breaks preserved thinking; Claude Code already retries |
| One policy core everywhere | `gateway-core` as a WASI component, identical in server, edge, and `wye explain` | Research; M6 |
