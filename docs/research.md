# Research

Open questions, ideas, and experiments; nothing here is a product promise. Measured results: [benchmarks.md](benchmarks.md). Status and build order: [roadmap.md](roadmap.md).

## Questions to test

- How fast does a disconnect release upstream work, and does the provider stop generation or billing?
- How much does cache-affine selection raise cache hit rate and lower billed cost on real agent workloads?
- Which results hold only against the mock?

A proxy adds a hop and cannot beat direct access on latency. Real-provider experiments are opt-in and record model, usage, cache behavior, pricing, and billed cost.

## Verification needed

Provider facts a feature depends on; each is confirmed, with a source, before that feature ships. Each entry: the feature that depends on it, the assumption, and how to check it. The cost bound and agent-aware routing list their own below.

- **[Provider-held budgets](policy.md#provider-held-budgets)**: which providers enforce workspace spend limits as hard stops, with what lag. Check: a low-limit project driven past its limit.
- **M0**: Claude Code's gateway contract per release. Check: [gateway guide](https://code.claude.com/docs/en/llm-gateway-protocol); contract tests per release.
- **M3**: Codex's transport and base-URL support for Responses, read from Codex's source (970111f) and gateway guide on 2026-10-26 ([architecture.md](architecture.md#codex-integration)). Check: one real Codex session through Wye.
- **Responses cost**: OpenAI `input_tokens` include `cached_tokens`; reasoning tokens are part of `output_tokens`. Check: OpenAI API reference; one recorded response with a cache hit.
- **`include_usage` default**: which SDKs tolerate the extra empty-`choices` usage chunk. Check: SDK conformance tests.
- **Bedrock provider** (built with `x-api-key`): Bedrock's Messages endpoint takes its API key in `x-api-key` (Anthropic's page) or `Authorization: Bearer` (AWS's page), and its `anthropic.`-prefixed ids for every current model. Check: one request with each header and a recorded stream.
- **Signing streamed bodies** ([M4](architecture.md#claude-across-providers-and-upstreams)): Bedrock's Messages endpoint and Claude Platform on AWS accept SigV4 with `UNSIGNED-PAYLOAD`. Check: one signed request per endpoint.
- **M4**: Vertex's streaming URL and framing; `count_tokens` and `anthropic-beta` on each Claude provider. Check: platform docs; one recorded stream per provider.
- **Codex for one developer without an OpenAI key**: Codex on a ChatGPT login can go through a loopback gateway: its endpoint, request encoding, and terms of use. Check: Codex's source; OpenAI's terms; one session through a loopback gateway.
- **Failover without duplicate billing**: `429`/`529` from each Claude provider means the request was not processed. Check: provider docs; billing records.
- **[Upgrade checks](architecture.md#model-aliases-and-upgrade-checks)**: each rule (a field a model rejects), built from Anthropic's migration guide (2026-10-26). Check: one opt-in live request per rule.

## Related work

Each entry: the work, its core idea, and its fit with no shared state. Work that needs shared state is under [rejected approaches](#rejected-approaches).

- ***The Tail at Scale***: hedged requests. Fit: request-local; risks duplicate billing.
- **Netflix `concurrency-limits`**: RTT-derived adaptive admission. Fit: needs cross-request local state.
- **Maglev / rendezvous hashing**: stable selection under membership change. Fit: deterministic; the basis of `cache_affine`.
- **FrugalGPT / RouteLLM**: offline quality-aware model selection. Fit: if the policy is static at runtime.
- **Macaroons / Biscuit**: offline attenuated authorization (team → project → subagent) without contacting an issuer. Fit: authorization, not cumulative usage; see [narrowable tokens](#3-narrowable-tokens-for-agent-swarms).
- **[TraceLab](https://arxiv.org/abs/2606.30560)** (2026): ~4,300 Claude Code and Codex sessions: long loops, long contexts, short outputs, imperfect cache hits. Fit: benchmark workload; evidence for cache-miss diagnostics.
- **[CacheWise](https://huggingface.co/papers/2606.16824.md)**, **[Agentic Coding in the Wild](https://www.microsoft.com/en-us/research/publication/agentic-coding-in-the-wild-characterizing-github-copilot-at-production-scale/)** (2026): agent traces: large repeated prefixes; loops coupled 1:1 with tool calls. Fit: uploads and per-event overhead matter, not bulk throughput.
- **[CacheRoute](https://arxiv.org/pdf/2608.19677)**, **[KV-cache-aware load balancing](https://www.arxiv.org/pdf/2601.18999)** (2026): prefix-affinity routing: cache hits 64% → 93%; affinity can cost capacity. Fit: motivates a stateless load bound on `cache_affine`.
- **[R2-Router](https://arxiv.org/abs/2602.02823)**, **[When Routing Collapses](https://arxiv.org/abs/2602.03478)**, **[ContextualRouter](https://aclanthology.org/2026.eacl-srw.22)** (2026): route by model and output budget; routers drift to the strongest model; kNN matches complex routers. Fit: simple routing tables fit static config.
- **[IETF Attenuating Agent Tokens](https://datatracker.ietf.org/doc/html/draft-niyikiza-oauth-attenuating-agent-tokens-00)** (draft): JWT-based offline attenuation for agent delegation. Fit: a standard format for narrowable tokens.

Summaries come from abstracts; check figures against the papers before citing them.

## Feature directions

Request-local, no shared state, config in Git, analysis in the user's OTel backend. Built, and documented where the feature lives: 2, [cache-miss diagnostics](architecture.md#cache-diagnostics); the CI half of 5, `wye policy-diff`; the clean stream endings of 7 ([failure behavior](operations.md#failure-behavior)); and [aliases and upgrade checks](architecture.md#model-aliases-and-upgrade-checks).

### 1. Agent-aware routing

Decided (2026-11-01): the agent keeps the phase decision, since only it knows the phase (Claude Code does not signal plan mode to a gateway), and Wye holds the request-local policy around it. That is aliases, model permissions, and the allowlisted `model` rewrite within one model family, keyed on Claude Code's request-class and agent-type headers and fitted offline from the cost per [request class](glossary.md): first [size-aware model routing](glossary.md) for small auxiliary requests ([M2c](roadmap.md#m2c-fewer-expensive-tokens)), later by subagent type, on every turn of that subagent so its cache survives. Before any default: `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`, each class's share in opt-in telemetry, and its quality on a smaller model in a task-level evaluation. Not proposed: classifying prompt content, or switching a conversation's model mid-session.

Why: agents already route by phase (Claude Code's [`opusplan`](https://code.claude.com/docs/en/model-config) and [subagent models](https://code.claude.com/docs/en/sub-agents), [Codex](https://learn.chatgpt.com/docs/config-file/config-reference), [Aider](https://aider.chat/docs/usage/modes.html), [Cline](https://docs.cline.bot/core-workflows/plan-and-act)); routers that classify each request (Cursor, [Copilot](https://docs.github.com/en/copilot/concepts/models/auto-model-selection), [OpenRouter](https://openrouter.ai/docs/guides/routing/routers/auto-router), [LiteLLM](https://docs.litellm.ai/docs/proxy/auto_routing)) pin a session or switch only at cache boundaries, because each model has [its own cache](https://code.claude.com/docs/en/prompt-caching); and 2026 studies find routing needs the agent's own trajectory ([SWE-Router](https://arxiv.org/abs/2607.00053), [Agent-as-a-Router](https://arxiv.org/abs/2606.22902)).

### 3. Narrowable tokens for agent swarms

Planned as [narrowable tokens](glossary.md): a token holder mints a narrower gateway token offline (team → developer → CI job → subagent), narrowing models, output-token ceilings, and expiry. The IETF attenuating-agent-tokens draft specifies this as JWTs, matching Wye's format. Caps each request, never cumulative spend; replay within the validity window stays possible.

### 4. Signed usage receipts

Proposal: a signed receipt per request (subject, session, model, status, reported usage, the provider's `request-id`, `prev` = hash of the previous receipt from the same process), as a compact JWS with `alg` pinned to `EdDSA`, in the usage log and OTLP. Each process keeps its own hash chain, so deletion breaks a chain without coordination; `wye receipt verify` checks signatures and gaps offline. No body hash: SHA-256 of 1 MiB takes 2.16 ms, more than Wye's whole upload overhead. Overhead: 13.7 µs per signature in the writer task, ~1.5% of a core at 1,000 requests/s. Attests what Wye forwarded and what the provider reported, not what it billed.

### 5. Queryable and provable policy

Zelkova-style analysis over static config, beyond `wye explain` for one request. Built: the CI check, [`wye policy-diff`](operations.md#commands). Open: the question form ("who can reach Opus?", "which subjects or grants reach this model?"), which needs the token side as data.

### 6. Provable cost bound

Planned for [M2](roadmap.md#m2-cost-policy-for-claude), not built; the rule it serves is in [policy.md](policy.md#provable-cost-ceiling). A bound without a tokenizer, so without per-provider tokenizer dependencies or network calls, from the [price table](policy.md#price-table) and each model's context window:

```text
max_input_tokens = min(input_bytes, model_context_window) + provider_overhead
max_cost = max_input_tokens × highest_input_price_reachable
         + output_cap       × output_price_per_token
```

- Input: a byte-level BPE token covers at least one byte, so input tokens ≤ UTF-8 bytes; the body's size over-counts (JSON syntax included), and the context window caps it, since the provider rejects a larger request. Verify per tokenizer, template tokens included: its construction, and adversarial requests against reported usage.
- Output: Wye would insert or lower the output-token field to `output_cap` ([allowlisted rewrite](architecture.md#forwarding-contract)), which must count reasoning tokens. Verify per model: low limits with reasoning on, against usage.
- Images: add each model's per-image token maximum (Anthropic bills by pixel dimensions, capped after resizing) instead of counting base64 bytes.
- Cache: reads bill at or below the input price and are ignored; writes use the highest write price the request can trigger (a 1-hour TTL above 5 minutes).
- Prompt-length tiers: price the whole bound at the tier its worst case (input bytes plus `output_cap`) reaches.
- Provider-added input, such as chat-template tokens and the tool-use system prompt `tools` adds: add each documented overhead. Verify it, the image maximum, and the tiers from provider docs and requests with and without images and `tools`.
- Refused in hard mode: audio and files (priced by separate formulas), hosted tools billed per call, priority or other service tiers, any field the price table does not cover, images until their maximum is verified, and any model whose bounds are unverified.

Loose on purpose, it always holds; telemetry would report it beside the actual cost. Agent traffic reads most input from the cache at 5 to 10 percent of the input price while the bound assumes writes, so it can exceed actual cost by an order of magnitude: a guard against runaway requests (an enormous context at the most expensive tier, an unbounded output), not a budget. Open: bound ÷ actual cost on real Claude Code sessions (opt-in telemetry), and a tighter bound that subtracts non-billable framing.

### 7. Reliability

- Connect-only hedging: race a second connection attempt, never a second request.
- Deterministic simulation testing: Wye, mock, and clients in a simulated network with seeded faults.
- Proof-of-possession tokens: bind gateway tokens to a client key.

### Agent-native directions

Built: what Claude Code tells a gateway without exposing prompts ([contract](architecture.md#claude-code-gateway-contract), [prompt caching for agents](https://arxiv.org/abs/2601.06007)) feeds attribution by session, subagent, and request class, cache-miss reasons, one trace per session (checked in Jaeger across two replicas), and tool calls. Open: report-only secret detection, which needs a multi-pattern matcher and a sampled body scan; report-only, because blocking would brick a session and redaction would break caching.

### Visibility for one developer

Research (2026-10-30): what only a gateway can add for a solo developer, without state between requests; today's tools: [positioning](product.md#positioning). Nobody prices what-ifs for the prompt cache, the carried cost of tool output, or a per-prompt bill; the first two are the [M2c](roadmap.md#m2c-fewer-expensive-tokens) waste report. Ideas beyond it, in the log writer task or an offline CLI, never on the request path:

- **A per-prompt bill with labels.** Main, subagent, and auxiliary requests rolled up per prompt, labeled with the prompt's first line by joining Claude Code's local transcripts (which carry `promptId`) in the CLI, so prompt text never enters the log.
- **`wye statusline`.** Reads the end of the log for the status line's session and prompt: this prompt's cost with its subagents and retries, the last cache rebuild, the costliest carried tool. The delivery channel for the waste report and the per-prompt bill.
- **Complete API-equivalent spend.** Subscription value at API prices, including what transcripts miss; prices applied at report time would price old logs too (a log written without `[pricing]` has no `cost_usd`).
- **Failure summary.** 429, 529, and 5xx answers, cancellations, and time lost, with the expected session-start probe separated out.

Not worth building: live 5h/7d headroom, which Claude Code already reads from the headers Wye forwards. Unverified: the status line's `rate_limits` behind Wye (likely, for the same reason).

### Where the tokens go

Measured on 2026-11-01 from one developer's log, the evidence behind [M2c](roadmap.md#m2c-fewer-expensive-tokens): 595 Claude Code requests in 3 sessions, all on Opus 5.5 through a claude.ai subscription. Spend is in units of the input price at Opus 5.5's [ratios](https://platform.claude.com/docs/en/about-claude/pricing): output 5, cache read 0.05, cache write 2 on the main conversation (Claude Code's 1-hour TTL on a subscription, [caching](https://code.claude.com/docs/en/prompt-caching)) and 1.25 elsewhere (5 minutes); the log does not split writes by TTL, so those follow Claude Code's defaults. One developer and one model: the shares will differ by workload.

| Spend | Share |
|---|---|
| Cache reads (97.6% of prompt tokens) | 43.2% |
| Of those, main-thread context beyond 200k tokens | 20.7% |
| Cache writes | 31.3% |
| Of those, 5 requests rebuilding large conversations | 19.1% |
| Of those, other session and subagent starts | 2.5%, expected |
| Output | 18.3% |
| Uncached input | 7.3% |
| Auxiliary requests (176, all on Opus) | 14.8% |
| Of those, the 141 under 50k tokens | 10.8% |

The median main request carried 400k tokens; sessions ran to about 968k before their one compaction. Four of the 5 rebuilds came after 7.5-15.6 idle hours, past any cache TTL, at 181k-497k tokens each; the fifth opened a session with 879k.

- Shortening or compressing prompts at the gateway would raise the bill and break report-don't-rewrite: at Opus 5.5's prices, a changed byte in a cached prefix turns reads at 0.05 into writes at 1.25 or 2, 25 to 40 times the price.
- What can be cut: context carried from turn to turn, conversations resumed after a break (only the user or agent can act, by clearing, compacting, or starting fresh first), rebuilds with a cause to fix, and the main model on small background work (on a subscription, Claude Code runs every background request on it, [source](https://code.claude.com/docs/en/llm-gateway-protocol)). Only small requests should move: each model has its own cache, so a large auxiliary request, which reads the main thread's, would rebuild that context at write prices.
- TTL advice must be computed per user, from timestamps: this user already had the 1-hour TTL and lost the cache after hours, while an API key defaults to 5 minutes. Idle time past the TTL is visible only across requests; one request cannot tell that it starts a session or a subagent, so its record says `cache_miss: unexplained`, and `wye report` counts those first writes as starts.

## Rejected approaches

Each needs state shared across requests or replicas, which Wye does not keep ([state boundary](policy.md#state-boundary)); the list of what Wye will not do is in the [README](../README.md#what-wye-will-not-do).

- Semantic or response caching, global rate limits, redemption-based tokens, and adaptive per-node concurrency.
- Loop detection or spend caps per session (cross-request state), prompt rewriting for cache efficiency (beyond the [allowlist](architecture.md#forwarding-contract)), and blocking tool calls (agent control is a non-goal).
- Signed balances: if a token says `$20 remaining` and Wye returns a new `$18` token, a client can still reuse the original on another replica, since signature verification does not track redemption. This is the double-spending boundary known from Privacy Pass and offline e-cash.
- A delayed cutoff: feeding OTLP usage to an external aggregator that triggers a GitOps policy rollout gives monitoring, not a budget; export loss, aggregation windows, rollout delay, and in-flight requests all cause overshoot.
- Redis, PostgreSQL, or SQLite ledgers: solve global quotas but add state and an operational dependency; SQLite makes even one instance stateful.
- Peer-to-peer actors: ownership, replication, fencing, quorum, and failover keep state inside the gateway; total cluster loss destroys unpersisted balances.
- Escrow or quota leasing: reduces coordination but still needs durable ownership and recovery.
- Gossip or bounded-counter CRDTs: converge eventually but cannot cap concurrent overspend without coordination.
- RLN, nullifiers, Privacy Pass, offline e-cash, one-time credentials, hash chains, proof of work: preventing or detecting double spending needs remembered redemption state or a trusted counter.
- Trusted hardware: could hold monotonic state client-side but is impractical as a default for Codex or generic applications.

## Performance techniques

Each is a hypothesis until `wye-bench` measures it ([method](benchmarks.md#method)); one line per verdict, the full runs in git history.

### Profiles

- Plain path: almost all system calls (`writev`, `recvfrom`).
- TLS path: dominated at first by h2's per-connection mutex, until streams were [spread over 64 connections](#adopted).
- Paced load, TLS (2026-10-24, `sample`, 10 s of 2,000 streams), of busy samples: `writev` to clients 43% (one write per event, inherent at token pace), `recvfrom` from the upstream 16%, waits on h2's per-connection lock 10% (`poll_data`, `is_end_stream`), Wye's own code under 1%.
- Memory per stream (2026-10-25, `heap` and `malloc_history`, 2,000 paced TLS streams), of the ~24 KB of heap: hyper's HTTP/1.1 client connection holds a fixed 8.5 KB read buffer and an 8.5 KB write buffer (70%), its dispatcher 3.5 KB, the Tokio task 0.9 KB, request headers 0.7 KB, timer and service closure 0.2 KB; hyper 1.x exposes only a cap on those buffers (`max_buf_size`, set to 32 KiB), so shrinking them needs a hyper change.

### Adopted

- **HTTP/2 to the provider**: negotiated by ALPN (verified against `api.anthropic.com`).
- **Spread streams over 64 connections**: TLS CPU per MB 12.3 → 4.1 ms, unary added latency 1.5 → 0.3 ms, throughput 4×; RSS after load 10 → 30 MB.
- **Uncompressed responses from the upstream** (`accept-encoding: identity`, 2026-10-17): in the first real session every stream was recorded `usage_status: unknown`, since Claude Code asks for `gzip, deflate, br, zstd`, Anthropic answers Brotli, and the mock never compresses. From 353 real responses rebuilt as streams and compressed with a flush per event, at 8, 16, and 32 B deltas: identity 35.9, 21.5, 14.3 KB per response (151, 173, 211 B per event); gzip -6 compresses 4.0, 3.5, 3.1×, Brotli q4 5.7, 4.5, 3.6× (q11 0.1× more), zstd -3 5.8, 4.6, 3.7×. So identity adds 10-30 KB per response, ~28 kbit/s while streaming: 0.12 ms per event on a 10 Mbit/s link against a 20 ms token gap. Decompressing a copy was rejected: 6-11 µs per event (20-90% of Wye's relay overhead), an attacker-sized decoder window per stream, and allocations per event. Why `identity`: [architecture.md](architecture.md#forwarding-contract).
- **OpenAI usage scanners: whole field name per line, searchers built once per stream** (2026-10-26, release micro-benchmark, 200,000 token-sized events, best of 7): Responses 89 → 34 ns per event, Chat Completions 106 → 40 ns; current numbers in [benchmarks.md](benchmarks.md#current-harness-and-baseline).
- **JSON usage scanner on unary responses** (2026-10-26, when the mock's unary replies became `application/json`, as real ones are): one allocation per response; `alloc_unary_per_request` 20.6 → 21.6, `alloc_unary_bytes_per_request` 21.8 → 24.4 KB, `unary_cpu_us_per_request` 120 → 124.5 (within noise).
- **Maximum HTTP/2 connection window, 256 KiB per stream** (bug fix, 2026-10-24): with hyper's defaults (5 MiB connection, 2 MiB stream), 200 stalled clients made 643-811 other streams fail, now 0; RSS per stalled stream 3.3 MB, then 2.0 MB at 1 MiB (the first fix), then 0.49 MB; bulk CPU per MB +10% against 1 MiB (4.06 → 4.47 ms), token streams unaffected.
- **Client write-stall timeout**: a connection whose writes make no progress for `client_write_timeout_secs` (60) is closed, ending a stalled stream; no overhead while writes never block, since the timer is allocated at the first blocked write.
- **Per-connection token cache**: CPU per authenticated request -38% and -44% in two A/B runs, equal to unauthenticated; ~0.3 KB per connection.
- **Shared cached identity (`Arc`) and borrowed log line**: allocations per authenticated request 37.5 → 20.6.
- **32 KiB client-connection buffer cap** (approved tradeoff): idle TLS stream after 512 KB 150 → 49 KB; plain uploads +7% CPU; 16 KiB added 50% plain upload CPU.
- **Client socket send buffer fixed at 128 KiB** (2026-10-29): autotuning let a non-reading client's kernel send buffer grow to 4 MiB, outside Wye's bounds, until the stalled-client test exhausted a 7 GB macOS CI runner's 96 MB network pool; now 32 MB for 256 stalled clients (64 MB on Linux, which doubles the value set); A/B: bulk CPU per MB +7.6% TLS (within its noise), -1.3% plain, other overhead unchanged.
- **Boxed request-body state**: holding 88 bytes inline added 2.8 KB per idle plain stream (hyper stores 32 requests per connection inline).
- **SIMD `memchr` usage scanning**: a byte loop added 49% CPU per MB (blocked by the gate); `memchr` +5.8%.
- **Forced inlining of the prompt lexer**: ~20% faster scanner.
- **Fast cancellation as a metric**: gated; release ~80 µs p50, < 0.2 ms max.

### Rejected or not adopted

- **256 lanes instead of 64**: rejected (2026-10-24, two alternating 20 s paced runs): CPU per event +3.5% and +8%, idle RSS +18%; ~10% of busy samples waited on h2's per-connection lock, but more connections add more overhead than the contention they remove.
- **Adaptive write coalescing**: rejected, no gain.
- **Lazy idle deadline**: rejected, no gain.
- **SWAR string-end search**: rejected, no gain over a byte loop.
- **mimalloc**: rejected: idle memory +9%, RSS +12%, CPU per MB +4%.
- **Failover: copy kept bodies into one buffer per request**: rejected (2026-10-26, `failover_` variant, two runs): allocations per agent upload 99.5 → 58.5, but CPU per MB 1.86 → 2.08 ms (agent upload) and 1.81 → 1.97 ms (1 MiB upload), heap 1.12 → 1.55 MB per agent upload, +1 allocation per unary request; the kept chunks stay ([their overhead](benchmarks.md#current-harness-and-baseline)).
- **SIMD stage-1 prompt scanner** (cache diagnostics): not adopted: the scalar scanner runs 0.61-0.69 ms/MB on Claude Code-shaped bodies, within 15% of a naive lexer (0.54); the prototype classified at 0.17-0.24 ms/MB, but a full scanner would land near 0.35-0.4 ms/MB, still above the 10% target, with the first `unsafe` code on the request path; per-session sampling instead.
- **Fat LTO, one codegen unit**: not adopted: CPU per MB 6-8% lower, but worse event p99 in both A/B runs, 2× build time.
- **Thin LTO**: not adopted (2026-10-30, two A/B runs): CPU 2-6% lower on nearly every metric, below the gate's sensitivity, and no event p99 penalty; but cache-diagnostics upload CPU per MB +5.9% / +2.8%, and a release rebuild after a `gateway-core` change 26-32 s instead of 18.5 s, paid several times per gate run; needs that tradeoff approved.
- **Transparent stream repair**: rejected: changes semantics and billing; Claude Code retries.

### Other verdicts

- **One h2 lock per frame**: candidate; A/B inconclusive (-5%, +12%).
- **Fewer runtime threads**: a deployment knob (`TOKIO_WORKER_THREADS`): CPU per MB 3.6 → 1.7 ms from 16 to 1 thread, but latency under load rises (2026-10-25, TLS, single runs, 16 / 4 / 2 workers: per-event delay p50 318 / 266 / 305 µs, CPU per paced event 38 / 25 / 18 µs, unary added p50 237 / 690 / 823 µs).
- **Pre-warmed connections**: low value: the 32 first requests on a new process add about 3 ms p50 at the baseline and 3-9 ms under load ([benchmarks](benchmarks.md#current-harness-and-baseline)), mostly first execution of the TLS and h2 code; after one warm-up request ~2 ms (concurrent handshakes), and a single cold connection adds nothing.
- **kTLS + splice; thread-per-core runtime**: research only.
