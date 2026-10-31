# Research

Open questions and directions; nothing here is a product promise. Measured results: [benchmarks.md](benchmarks.md).

## Questions to test

- How fast does a disconnect release upstream work, and does the provider stop generation or billing?
- How much does cache-affine selection raise cache hit rate and lower billed cost on real agent workloads?
- Which results hold only against the mock?

A proxy adds a hop and cannot beat direct access on latency. Real-provider experiments are opt-in and record model, usage, cache behavior, pricing, and billed cost.

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
| Bedrock's Messages endpoint takes its API key in `x-api-key` (Anthropic's page) or `Authorization: Bearer` (AWS's page), and its `anthropic.`-prefixed ids for every current model | Bedrock provider (built with `x-api-key`) | One request with each header and a recorded stream |
| Bedrock's Messages endpoint and Claude Platform on AWS accept SigV4 with `UNSIGNED-PAYLOAD` | Signing streamed bodies ([M4](architecture.md#claude-across-providers-m4)) | One signed request per endpoint |
| Vertex's streaming URL and framing; `count_tokens` and `anthropic-beta` on each Claude provider | M4 | Platform docs; one recorded stream per provider |
| `429`/`529` from each Claude provider means the request was not processed | Failover without duplicate billing | Provider docs; billing records |
| Each upgrade-check rule (a field a model rejects) | [Upgrade checks](architecture.md#model-aliases-and-upgrade-checks) | Built from Anthropic's migration guide (2026-10-26); one opt-in live request per rule still open |

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

A1-A4 are built ([status](roadmap.md#status)). Measured: agent-header capture 0.04-0.32 µs per request, validation 0.11 µs in the writer task, tool names from `content_block_start` 0.22-0.33 µs per block with zero allocations. Sessions are one flat trace, checked in Jaeger across two replicas: no synthetic parent spans, since no request knows it is first.

Open: A5, report-only secret detection. It needs a multi-pattern matcher and a sampled body scan. Report-only, because blocking would brick a session and redaction would break caching.

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

Each is a hypothesis until `wye-bench` measures it ([method](benchmarks.md#method)).

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
