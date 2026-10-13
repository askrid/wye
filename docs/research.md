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
| Which SDKs tolerate the extra empty-`choices` usage chunk | `include_usage` injection default | SDK conformance tests in Python, TypeScript, Go, and common agent frameworks |

## Benchmarks

**Mock upstream first:** fixed event timing and payloads, varied concurrency, slow readers, deliberate disconnects.

- **Baselines:** direct mock upstream, Wye, and LiteLLM, OmniRoute, or KrakenD only in a feature-equivalent streaming configuration.
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

Run-to-run noise on this machine is about 10–15% for CPU per MB and larger for tail latency, and ±25% on the TLS path, so the gate reliably catches regressions of roughly 25% or ~100 µs, not smaller ones. The paced scenario saturates the whole laptop, so its latencies include machine load. Not covered yet: gateway credentials enabled (verification costs about 55–62 µs per request in isolation), Linux, and multi-machine load.

**Profiles.** On the plain path, Wye's CPU is almost all system calls (`writev` and `recvfrom` per frame, connection setup); its own code is a small share. On the TLS path, profiles were first dominated by lock contention in `h2`, which guards all streams of one connection with one mutex; spreading streams over connections ([architecture.md](architecture.md#upstream-connections)) cut CPU per MB from 12.3 to 4.1 ms and unary added latency from 1.5 to 0.3 ms. Remaining CPU gaps come from relaying each frame as it arrives and from handing each chunk between the upstream and downstream connection tasks.

### Against KrakenD

Goal: beat KrakenD on the benchmark. `wye-bench run ... --krakend <path>` runs every scenario through KrakenD 3.0.1 CE as well, in its documented pass-through mode (`no-op` encoding, all headers and query strings, one backend) and its best case (no access log, ERROR logging), while Wye keeps its per-request usage log.

**KrakenD CE cannot stream SSE.** In `no-op` mode it buffers the whole response: an event sent at 0 s arrived when the stream ended, 3 s later. `"sse"` encoding is Enterprise-only; CE answers it with 500. For Claude Code that means no tokens appear until the reply is complete. The idle-memory and cancel scenarios, which wait for the first event of a silent stream, therefore run only for Wye.

Median of 3 on the development laptop, all 16 cores:

| Metric | Wye | KrakenD |
|---|---|---|
| Added unary latency p50 / p99, plain | 224 / 106 µs | 456 / 1,421 µs |
| Added unary latency p50 / p99, TLS | 243 / 373 µs | 365 / 1,509 µs |
| Added delay per SSE event p50, TLS | 0.22 ms | 33.8 ms (buffered) |
| Added time to first byte p50, TLS | 0.33 ms | 69 ms (buffered) |
| RSS after load, TLS | 29 MB | 106 MB |
| CPU per streamed MB, TLS | 4.0 ms | 1.6 ms |
| Bulk throughput, TLS | 1.1 GB/s | 2.2 GB/s |

With equal CPU budgets (`TOKIO_WORKER_THREADS` / `GOMAXPROCS`), Wye still wins streaming and memory, and unary latency at 2 threads (TLS p50 828 vs 1,224 µs); at 4 threads TLS unary is a tie (715 vs 679 µs). KrakenD keeps a 1.5–2× lead on bulk CPU per MB (4 threads: 1.43 vs 2.29 ms plain, 3.16 ms TLS; 2 threads: 1.02 vs 2.1 ms).

**Relevance.** Bulk throughput, the one area KrakenD leads, matters least for LLM traffic: streams are token-paced (KB/s), and Wye's added 0.2–0.3 ms is under 0.1% of a typical model response. What matters to users is streaming without buffering, concurrency without queuing, memory per idle stream, cancellation, and p99 under load; the [realistic-workload benchmark](roadmap.md#realistic-workload-benchmark) measures those workloads.

**Realistic workloads** (`upload_*`, `tls_paced_*`; median of 3):

| Workload | Metric | Wye | KrakenD |
|---|---|---|---|
| 1 MiB uploads, concurrency 16, TLS | added latency p50 / p99 | 3.0 / 3.7 ms | 4.0 / 9.8 ms |
| | CPU per uploaded MB | 2.5 ms | 2.5 ms |
| 2,000 streams × 50 events/s × 100 B, TLS | added delay per event p50 / p99 | 2.1 / 3.8 ms | 296 / 569 ms (buffered) |
| | CPU per event | 37 µs | 86 µs |
| | memory per active stream | 5.9 KB | 74 KB |

The paced scenario loads the whole laptop (about 100k events/s, with mock, load client, and gateway sharing 16 threads), so its latencies include machine saturation; CPU per event and memory per stream are the precise signals. Stream starts are ramped over 2 s because macOS caps the listen backlog at 128; it runs only on the TLS + HTTP/2 path because over HTTP/1.1 every stream opens its own connections and repeated runs exhaust ephemeral ports. Unmeasured so far: Linux, multi-machine load, and a streaming-capable competitor.

### Against LiteLLM

`wye-bench run ... --litellm <path>` runs every scenario through LiteLLM 1.105 (Python 3.14), in its leanest proxy configuration: the byte-level pass-through endpoint (`pass_through_endpoints`, `forward_headers`, mounted at `/pt/v1/messages` because LiteLLM's own `/v1/messages` parses and translates requests), no database and therefore no spend logging, one worker per core (16), upstream CA through `SSL_VERIFY`. It streams SSE, so every scenario runs. Its CPU and memory are summed over the whole process tree.

Single run on the development laptop; zero failed requests or streams on either gateway:

| Metric | Path | Wye | LiteLLM |
|---|---|---|---|
| Added unary latency p50 / p99 | TLS | 0.26 / 0.67 ms | 48 / 174 ms |
| Added time to first byte p50 | TLS | 0.46 ms | 32 ms |
| Added delay per SSE event p50 / p99 | TLS | 0.19 / 0.34 ms | 2.0 / 42 ms |
| 2,000 paced streams: delay per event p50 / p99 | TLS | 1.1 / 3.2 ms | 68 ms / 1.26 s |
| 2,000 paced streams: CPU per event | TLS | 31 µs | 319 µs |
| 1 MiB upload: added latency p50 | TLS | 5.2 ms | 61 ms |
| 1 MiB upload: CPU per MB | TLS | 2.9 ms | 59 ms |
| Memory per idle stream | TLS | 5.2 KB | 125 KB |
| Process memory at idle | TLS | 27 MB | 8.6 GB |
| Bulk throughput | TLS | 1.06 GB/s | 0.33 GB/s |

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

## Experiments

- **A. Streaming backpressure at scale.** Thousands of mixed-speed SSE clients without memory growth or starvation? Measure RSS, chunk delay, cancellation cleanup, fairness.
- **B. Cost-constrained tail latency.** Can price/capability routing or request-local hedging improve p99 TTFT without excessive duplicate charges? Compare fixed route, cheapest eligible, bounded fallback, bounded hedging.
- **C. Cost-aware routing.** Evaluate cost per successful task at a quality threshold, not token price. Fit weights offline; deploy fixed parameters as Git-managed policy.
- **D. Capability delegation.** Team → project → subagent attenuation with Biscuit or Macaroons. Measure verification overhead and ergonomics; document replay limits.
- **E. Native Tokio versus Go.** Feature-equivalent proxies under long-lived streaming load: CPU, memory, p99 overhead, cancellation behavior, not just RPS.
- **F. Edge versus native.** Same routing policy under native Tokio and a WASM adapter, accounting for provider distance and runtime limits.

## Techniques for the gateway workload

The workload is unusual: few, long-lived requests; tiny SSE events at token pace; time to first token matters more than throughput; and any duplicated request may be billed twice. Each technique below is a hypothesis until `wye-bench` measures it. Status reflects measurements so far.

| Technique | Idea | Status |
|---|---|---|
| HTTP/2 to the provider | Multiplex client streams over few upstream connections: fewer TLS sessions, less memory, fewer cold handshakes | **Already in effect.** The client negotiates HTTP/2 by ALPN; 12 concurrent requests to `api.anthropic.com` shared one connection. Benchmark lacks a TLS/HTTP/2 mock, so its memory effect is unmeasured |
| Fast cancellation as a cost metric | When a client disconnects (Claude Code: Esc), release the upstream immediately, even while it is silent; every millisecond of delay can be billed output | **Measured and gated**: 77 µs p50, 157 µs max with a silent upstream (`cancel_upstream_release_*`). Provider-side effect (does generation and billing stop?) still to verify |
| Zero-copy usage extraction | Scan forwarded SSE bytes with SIMD `memchr`; parse JSON only in `message_start`/`message_delta` | **Implemented.** Byte-by-byte scanning cost +49% CPU per MB and was blocked by the gate; with `memchr`, +5.8% CPU per MB in an A/B run |
| Pre-warmed connections and connect-only hedging | Keep warm TLS connections to each provider; race a second connect when one stalls, never a second request | Planned. Needs a TLS mock with configurable connect delay to measure cold-start TTFB |
| Adaptive write coalescing | Write events immediately when the client keeps up; batch backlogged events into one `writev` | Measure first: hyper may already batch. Target is the ~3× CPU gap to the bare mock path |
| Cache-affine routing by session header | Rendezvous hashing on `x-claude-code-session-id` keeps a session on one account and its prompt-cache discount, without parsing the body | Design ([architecture.md](architecture.md#cache-affine-routing)); needs multi-candidate routing (M4) |
| Closed-loop policy through GitOps | OTLP usage → offline fit of routing weights → pull request to `wye.toml` → CI policy tests → deploy | Research; keeps adaptation outside the stateless runtime |
| Tighter provable cost bound | Subtract non-billable framing (JSON syntax, base64 image bytes priced separately) in a single pass, keeping the bound provable | Research; measure bound ÷ actual cost |
| Deterministic simulation testing | Run Wye, mock, and clients in a simulated network (turmoil or madsim) with seeded loss, slow readers, mid-stream drops, shutdown | Planned for stage 3 lifecycle tests |
| Proof-of-possession tokens without replay cache | Bind tokens to a client key; accept signed request proofs within a short window | Research; cannot stop exact replay inside the window |
| kTLS + splice | Kernel TLS lets bytes move socket to socket without user-space copies | Research only: Linux-only, pays off only at high throughput, incompatible with usage extraction |
| Lazy idle deadline | Record a timestamp per frame and re-arm the idle timer only when it fires early, instead of resetting the timer wheel when a stream starts waiting | **Rejected (measured).** No gain in A/B; under load the stream rarely waits, so the old code rarely reset the timer |
| Fat LTO + one codegen unit | Whole-program optimization of the release build | **Not adopted (inconclusive).** Two A/B runs: CPU per MB −8% and −6%, binary 5.8 → 3.7 MB, but SSE per-event p99 worse in both (+266%, +80%) and release builds about 2× slower. Revisit with a dedicated tail-latency investigation |
| Spread streams over connections | Least-loaded lane selection instead of packing up to 100 streams behind one `h2` mutex | **Adopted.** TLS: unary added latency −80%, CPU per MB −66%, throughput 4×; process RSS after load 10 → 30 MB with up to 64 connections |
| Adaptive write coalescing | Merge already-available frames into one write | **Rejected (measured).** No CPU gain; hyper already batches ready frames into one `writev` |
| Fewer runtime threads | Size Tokio workers to the CPU budget | **Measured, not adopted as default.** CPU per MB 3.6 → 1.7 ms from 16 to 1 thread, but unary latency under load rises; a deployment knob (`TOKIO_WORKER_THREADS`) |
| Multiple HTTP/2 connections per upstream | hyper-util's pool keeps one HTTP/2 connection per host; requests beyond the provider's stream limit queued silently | **Adopted (correctness).** 64 lanes with atomic slot admission, `upstream.max_streams_per_connection` (default 100). A 4-stream TLS mock now serves 10 concurrent streams; before, the 5th hung |
| One HTTP/2 lock acquisition per frame | `h2` guards each connection's stream state with one mutex; the usage wrapper's extra `is_end_stream()` per frame takes it again. Record completion inside hyper's own `is_end_stream()` call instead | **Candidate, not adopted.** Strictly less work, but A/B on the TLS path was inconclusive (CPU per MB −5% then +12%); TLS noise is larger than the effect |
| mimalloc | Replace the system allocator | **Rejected (measured).** Memory per idle stream +9%, process RSS +12%, CPU per MB +4%, throughput −5% |
| Thread-per-core runtime | monoio or glommio instead of Tokio work stealing | Research only, if CPU per MB becomes the limit |
| Transparent stream repair | Resume a dropped stream with assistant prefill of the partial output | Rejected for the product: changes semantics and billing, breaks preserved thinking, and Claude Code already retries |
| One policy core everywhere | `gateway-core` as a WASI component, with identical decisions in the server, at the edge, and in `wye explain` | Research; aligns with M6 edge adapter |
