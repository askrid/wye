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

Run-to-run noise on this machine is about 10–15% for CPU per MB and larger for tail latency, and ±25% on the TLS path, so the gate reliably catches regressions of roughly 25% or ~100 µs, not smaller ones. The paced scenario saturates the whole laptop, so its latencies include machine load. **Heap allocations** (`alloc_` metrics, plain path, `alloc-stats` build; median of 3, spread under 1% except uploads):

| Metric | Value |
|---|---|
| Allocations per streamed SSE event | 0 |
| Allocations per stream setup | 23 |
| Allocations / allocated bytes per unary request | 20.6 / 21.6 KB |
| Allocations per authenticated request (cached credential) | 20.6 |
| Live heap per idle stream | 49 KB plain, 23 KB TLS |
| Live heap per idle stream after a 512 KB request | 77 KB plain, 48 KB TLS (150 KB TLS before the buffer cap) |
| 1 MiB upload: allocations / allocated bytes | 42 / 0.41 MB (1.38 MB before the buffer cap) |

Findings: the relay path allocates nothing per event. An idle stream holds about 49 KB of live heap while RSS grows only about 28 KB, because large connection buffers are allocated but not fully touched. The dominant production cost was different: after an agent-sized request, hyper's client-connection read buffer stayed grown for the stream's lifetime (150 KB per idle TLS stream after 512 KB); a 32 KiB cap brought it to 48 KB (see the techniques table). Per-request allocations fell from 33.5 to 20.6 by sharing the cached identity behind an `Arc` (an authenticated request now costs the same as an unauthenticated one) and serializing the usage log line from borrowed fields. Uploads allocated about 1.35× the body as receive buffers grew; with the 32 KiB buffer cap it is about 0.4×.

Gateway credentials (`auth_` metrics, plain path, per-request scenarios only): full verification costs about +88 µs of CPU per request (the Ed25519 check alone measured 55–62 µs in isolation); with the per-connection credential cache, authenticated requests on reused connections cost the same CPU as unauthenticated ones. Not covered yet: Linux and multi-machine load.

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

**Realistic workloads** (`upload_*`, `tls_paced_*`; single comparison run, so numbers differ slightly from the gated baseline above):

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

Research-backed functionality that fits the constraints: request-local decisions, no shared state, config in Git, analysis delegated to the telemetry backend. Together, 1–3 are the intended moat: a gateway that understands agent traffic, with no database. Cache-miss diagnostics (2) is implemented; the others are not.

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

### 5. Queryable and provable policy

- **Basis:** automated reasoning over access policies (AWS Zelkova for IAM, using SMT).
- **Function:** beyond `wye explain` for one request: "who can reach Opus?", "can any credential exceed $0.05 per request?", and a CI check that fails when a config change widens access.
- **Fit:** static config and routing as a pure function are what make the analysis possible.

### 6. Provable cost bound

Planned for M2 ([policy.md](policy.md#provable-cost-ceiling)): a guaranteed upper bound per request without a tokenizer. Research: tighten it by subtracting non-billable framing (JSON syntax, separately priced image bytes) in one pass, and measure bound ÷ actual cost on real sessions.

### 7. Reliability

- **Connect-only hedging** (*The Tail at Scale*): race a second connection attempt, never a second request, so tail latency drops without duplicate billing.
- **Deterministic simulation testing** (FoundationDB style): Wye, mock, and clients in a simulated network (turmoil or madsim) with seeded loss, slow readers, mid-stream drops, and shutdown.
- **Clean stream endings:** when the upstream stalls, emit an Anthropic `error` event before closing, so Claude Code shows a retryable overload instead of a dropped connection.
- **Proof-of-possession credentials** without a replay cache: bind tokens to a client key; replay inside a short window remains possible.

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
