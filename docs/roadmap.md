# Roadmap

Each stage leaves a runnable system with focused tests. Order of the next work: packaging (rest of M1), the rest of M2, then M4 and M3 ([product.md](product.md#next)). Client phases (P0–P3) are defined in [product.md](product.md#later).

## Status

| Milestone | State |
|---|---|
| M0: Claude Code stream proxy | Done and gated. Exit check partly run: one real session through subscription passthrough (2026-10-17) streamed correctly and exposed compressed responses, since fixed; the full check with an API key is open |
| Realistic-workload benchmark | Done: uploads, 2,000 token-paced streams, capacity per core, KrakenD, LiteLLM, and Bifrost comparisons |
| M1: team-ready Claude Code | Stage 4 done: Wye's own Ed25519 credentials, model permissions and output-token ceilings, identity-provider tokens, `apiKeyHelper` renewal ([operations.md](operations.md#credential-renewal)). Stages 5–6 done: usage scanner, OTLP logs, metrics, and traces, `wye claude`, `wye probe`, `upstream.api_key_file`, local OCI image with a smoke test. Open: published binaries and images, Compose example and the Swarm rolling-update check, the M1 exit check against a real team setup |
| M1b: agent-aware visibility | A1–A4 done (retry headers, `GET /v1/models`, spend by work type, cache-miss reasons, session traces, `wye report`, tool calls). A5 (secret detection) open: needs a dependency decision |
| M2: cost policy | Pricing done ([policy.md](policy.md#pricing-metadata)): cost, cost status, and pricing version in usage records, `wye.cost` in OTLP, dollars in `wye report`; cache-miss diagnostics done early (opt-in, sampled per session). Cost bound, provider-held budgets, `wye explain` cost output not started |
| M2b: model aliases and upgrade checks | Done: `[[aliases]]` with per-subject rollout (stage A) and upgrade checks with a sourced rule table, sampled body scan, `upgrade_issues` in records and OTLP, `explain --target-model`, `wye report` by client (stage B) ([architecture.md](architecture.md#model-aliases-and-upgrade-checks)). Exit check (a real alias move) open |
| M3: Codex | Not started |
| M4: Claude across providers and accounts | Designed ([architecture.md](architecture.md#claude-across-providers-planned-m4)); not started |
| M5–M6 | Not started |

## Milestones

### Realistic-workload benchmark

Bulk throughput is the least relevant metric for an LLM gateway; traffic is token-paced and requests are large. Before further optimization or competitor claims:

- **Large request bodies:** Claude Code resends its whole context every turn (hundreds of KB to several MB). Measure added upload latency and CPU per uploaded MB.
- **Token-paced streams at scale:** 1,000–10,000 concurrent streams at ~50 events/s with ~100-byte events. Measure CPU per streamed event, memory per active stream, and p99 time to first byte and per-event delay under that load. Make CPU per event the primary efficiency metric; keep bulk MB/s as a secondary one.
- **Competitors that stream:** LiteLLM first, in a feature-equivalent pass-through configuration. KrakenD CE stays as a reference but cannot stream SSE.

**Exit:** gated metrics for all three workloads, and a comparison table against at least one streaming-capable gateway.

**Status: done.** Uploads, token-paced streams, and the stress scenarios (cold connections, mixed load, stalled clients) are measured and gated on the TLS + HTTP/2 path. LiteLLM and Bifrost are the streaming-capable comparisons ([research.md](research.md#against-bifrost)); the next competitors are in M5.

### M0: Claude Code stream proxy (P0)

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| 1 | Workspace, `wye serve --config`, startup validation, environment secrets, health endpoints | Valid config starts; invalid config fails without exposing secrets |
| 2 | Anthropic Messages JSON and SSE forwarding (`/v1/messages`, `/v1/messages/count_tokens`); request bounds and [local resource bounds](policy.md#local-resource-bounds), connect/header/inactivity timeouts, a client write-stall timeout, backpressure, cancellation, no retries; configured API key or explicit loopback [caller-credential passthrough](policy.md#caller-credential-passthrough) | [Claude Code gateway contract](architecture.md#claude-code-gateway-contract) tests pass; upstream status, headers, and error bodies preserved; SSE bytes, including `ping`, arrive before upstream completion; disconnect releases upstream work; forwarded request is unchanged (no rewrites yet) |
| 3 | Lifecycle hardening and graceful shutdown (**done**: drain phase with failing `/readyz`, grace deadline that cancels remaining connections, `max_concurrent_requests` with 529) | Slow readers stay bounded; failure paths release resources; shutdown drains within a finite deadline |

The first reviewable implementation stops after stages 1–2. Resource bounds and cancellation are part of a correct streaming proxy, not later features; stage 3 broadens their verification and adds deployment lifecycle.

**Exit:** a real Claude Code session, with an API key and with a claude.ai login through passthrough, works through Wye with streaming, tool use, prompt caching, extended thinking, and token counting. Loopback only. No multi-provider claim.

**Status:** stages 1–3 are implemented and covered by integration tests against a mock upstream; the real-session exit check is partly run (see [status](#status)).

### M1: team-ready Claude Code

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| 4 | **4a done:** Ed25519 JWT credentials from `Authorization` or `x-api-key`, `wye token keygen`/`issue`, `max_credential_ttl_secs`, `[auth]` lifts the loopback-only restriction, subject in the usage log. **4b done:** model permissions from config and the credential's `models` claim, read from the held start of the body; output-token ceilings from config and the `max_output_tokens` claim, the stricter applying. **4c done:** identity-provider JWTs (`[[auth.issuers]]`: pinned algorithm, JWKS file, subject claim, claim → model grants). `apiKeyHelper` renewal examples done. Stage 4 is complete | Invalid, expired, wrong-audience, unknown-key, and unauthorized credentials rejected before contacting upstream; credential limits only narrow config; background-model requests covered by permissions |
| 5 | Usage observer (`message_start`, `message_delta`; **done**), minimal OTLP telemetry with developer and Claude Code session attribution (**done**: usage records as logs with session and subject attribution, cumulative request, duration, token, and active-request metrics, and one span per request joined to the caller's `traceparent`), OCI image (**done** locally with a smoke test; publishing open), baseline benchmark (**done**) | Usage extracted from split and oversized events without altering forwarded bytes; outcomes and latency observable without logging prompts or secrets; image smoke test and benchmark recorded |
| 6 | `wye claude`, prebuilt binaries, Compose example; `upstream.api_key_file` and `wye probe` for container platforms without Kubernetes ([operations.md](operations.md#without-kubernetes); **both done**, as is a first `wye claude`) | Wrapper launches Claude Code against Wye and cleans up; a Docker Swarm service updates without dropping streams |

Stage 4 is required before any non-loopback deployment.

**Exit:** a team runs Claude Code through a shared Wye with per-developer credentials and sees usage per developer and session in its existing OTel backend.

### M1b: agent-aware visibility

Turns the signals Claude Code sends gateways into attribution and explanations ([product.md](product.md#principles)). Each item is request-local, adds only header reads to the request path, and passes the commit gate. Background in [research.md](research.md#agent-native-directions):

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| A1 (**done**) | Retry hygiene: Wye's own 529s carry `retry-after` (integer seconds, at most 60) and `x-should-retry: true`. Model picker: `GET /v1/models` lists the models the caller's credential may use, in the shape Claude Code's discovery expects | Integration tests for both; Claude Code's picker shows only permitted models |
| A2 (**done**) | Spend by work type: request class, agent type, compaction trigger, agent and prompt ids in the usage log, OTLP logs, and `wye.tokens`. Expected versus unexpected cache misses from `x-claude-code-context-compacted` | Header values bounded and validated before they become attributes; metric series stay within their limit; gate unchanged |
| A3 (**done**) | Session traces: trace id derived from the session id; tool durations as tool spans; prompt and agent structure as attributes (no synthetic parent spans) | One Claude Code session appears as one trace in a reference backend (Jaeger) across two replicas |
| A4 (**done**) | `wye report` over the usage log; tool names and counts from `tool_use` events in the response scanner | `alloc_sse_per_event` stays at zero; report output tested on recorded logs |
| A5 | Report-only secret detection, sampled per session | Measured cost per MB recorded; dependency justified; values never leave the process |

**Exit:** a team sees, in its own OTel backend or with `wye report`, each Claude Code session as a structure (prompts, subagents, compactions, tools) with cost by work type and every unexpected cache miss flagged, with no gateway state and no gated metric worse.

### M2: cost policy for Claude

- Pricing in config ([format](policy.md#pricing-metadata)): cost, cost status, and pricing version in every usage record, `wye.cost` in OTLP, dollars in `wye report`, cache writes priced by TTL. **Done.**
- The [provable cost ceiling](policy.md#provable-cost-ceiling) as a runaway guard for Claude models whose bounds are verified, capped by the context window; everything else labelled estimated-only. Off by default on the Claude Code route.
- [Provider-held budgets](policy.md#provider-held-budgets): identity → Anthropic workspace key mapping and explicit budget-exhausted errors.
- `wye explain` and policy tests report the cost decision.
- [Cache-miss diagnostics](research.md#2-cache-miss-diagnostics): per-block prompt hashes in usage telemetry. **Done** (usage log and OTLP logs).

**Exit:** a team's monthly Claude spend is capped by Anthropic workspace limits with no Wye state, and every request reports a provable or explicitly estimated cost bound next to its actual cost.

### M2b: model aliases and upgrade checks

Takes model choice and model upgrades out of client code ([design](architecture.md#model-aliases-and-upgrade-checks)).

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| A (done) | `[[aliases]]` with per-subject rollout, resolved by the existing model hold and spliced in place; permissions and pricing on the resolved model; aliases in `GET /v1/models`; `requested_model` in records; `explain` and `policy-test` show resolution | Splice property-tested (only the value changes, `content-length` matches); non-alias requests unchanged and no gated metric worse without `[[aliases]]` |
| B (done) | Upgrade-check rule table for current Anthropic models with a test per rule; sampled body scan; `upgrade_issues` in records, `wye.upgrade.issues` in OTLP, `explain --target-model`, `wye report` by client | Each rule caught on a fixture, none on a clean request; scan cost per MB measured and within the diagnostics budget when sampled |

**Exit:** a team moves a model alias for all its services by one reviewed config change, after the upgrade check showed no failing client.

### M3: Codex (P1, P2)

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| 7 | Responses forwarding to OpenAI through the same transport path, tool-related event fixtures, usage extraction, `wye codex` | A real Codex workflow streams and calls tools through Wye; endpoint fixtures pass |
| 8 | Upstream-held state pinning for `previous_response_id` | Requests referencing stored responses reach the owning account or are rejected |
| 9 | Chat Completions passthrough to OpenAI for OpenAI-compatible agents and SDKs; `include_usage` policy | The OpenAI SDK uses Wye with equivalent unary and streaming behavior |

Agent-aware telemetry for Codex uses whatever signals Codex sends; the usage record, metrics, and `wye report` stay one model across agents.

**Exit:** a team runs Claude Code and Codex through one Wye, under one policy, and sees both in one telemetry model.

### M4: Claude across providers and accounts

One Claude Code fleet served by several Claude providers and accounts, chosen per request and failed over without state ([design](architecture.md#claude-across-providers-planned-m4)).

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| 10 | Routes with several upstreams of one provider (accounts), `ordered` and `cache_affine` selection by session, provider and account in telemetry | The same session reaches the same account from two replicas; shares follow configured weights |
| 11 | Failover on `429`, `529`, or connection refusal before any response byte, with a bounded held body | Mock tests: failover happens only before output, never on ambiguous `5xx`, bodies above the bound go without failover, no gated metric worse without failover configured |
| 12 | Claude Platform on AWS and Amazon Bedrock (Messages endpoint): model-id mapping, API-key auth, then SigV4 | Contract tests per provider against recorded responses; one opt-in real smoke test each |
| 13 | Google Vertex AI: model in the URL, `anthropic_version` splice, access tokens | Body rewrite property-tested (every other byte unchanged); opt-in real smoke test |
| 14 | Agent-aware model routing within Claude ([research](research.md#1-agent-aware-routing)), once its quality impact is measured; Helm chart, `wye doctor` | Request-class policy tested; quality measurement recorded |

**Exit:** a team serves Claude Code from at least two providers, keeps prompt-cache hit rates within a few points of a single provider, rides out one provider's overload without failed turns, and sees cost per provider.

### M5: benchmarks and optimization

- Reproducible SSE upstream emulator and load generator. **Started early:** `wye-bench` and the commit gate exist since M0, because speed is a release criterion for every milestone, not only M5 ([research.md](research.md#current-harness-and-baseline)). Already covered: TLS + HTTP/2 upstream, uploads, 2,000 token-paced streams, cold connections, mixed load, stalled clients, capacity per core, KrakenD, LiteLLM, and Bifrost. M5 adds a floor baseline (nginx or Envoy as a plain streaming proxy), Envoy AI Gateway, Helicone's gateway, and Portkey ([research.md](research.md#benchmark-suite-kept-cut-missing)); 10k streams; Linux and a separate load machine; and replay of the public TraceLab coding-agent trace ([research.md](research.md#related-work)).
- Equivalent baseline configurations and published methodology ([research.md](research.md#benchmarks)).
- Profiling and targeted optimization of memory, TTFT, backpressure, and CPU.

**Exit:** comparative results document actual performance, workload assumptions, and tradeoffs. No unsupported speedup goals.

### M6: optional extensions

Edge/WASM adapter, [delegable credentials](research.md#3-delegable-credentials-for-agent-swarms), [signed usage receipts](research.md#4-signed-usage-receipts), [queryable policy](research.md#5-queryable-and-provable-policy), connect-only hedging, and other request-local experiments.

**Exit:** each extension preserves or explicitly revises statelessness and compatibility guarantees.

## Tests

Deterministic integration tests use a local mock upstream. Normal CI never makes paid provider calls.

- Preserve request bodies, unknown fields, tool payloads, and provider authorization.
- Preserve upstream status, JSON errors, and permitted response headers.
- Deliver SSE bytes before upstream completion.
- Handle events and UTF-8 sequences split across arbitrary HTTP chunks.
- Bounded behavior under slow downstream consumption.
- Upstream resource release after downstream disconnect.
- Reject oversized requests; exercise timeout and upstream failure paths.
- Drain or cancel active streams according to the shutdown deadline.

Core unit tests and property tests cover configuration validation, credential verification, model permissions and token ceilings, and the request and SSE scanners; routing policy joins them when it is built. A small opt-in real-provider smoke test checks SDK compatibility; it does not replace deterministic protocol tests.

## Release acceptance checklist

### Functional / API

- [ ] Every supported endpoint × provider × feature combination has passing conformance and integration tests.
- [ ] The published support matrix is generated from those test results, not written by hand.
- [ ] P0 Anthropic Messages passes the Claude Code gateway contract tests for the target Claude Code version.
- [ ] P1 native OpenAI Responses streaming works with the target Codex version.
- [ ] P2 Chat Completions unary and streaming work with OpenAI.
- [ ] P3 each Claude provider passes the Messages contract tests; failover happens only before any response byte, never after output.
- [ ] Aliases resolve the same in `explain` and `serve`; upgrade checks report and never change a request.
- [ ] Unsupported provider/endpoint combinations reject explicitly.
- [ ] No unbounded buffering on SSE with a slow downstream.
- [ ] Cancellation frees gateway resources and does not claim to cancel already billed usage.

### Policy / security

- [ ] Signature, issuer, audience, expiry, and model permissions enforced before upstream dispatch.
- [ ] Config references and environment secrets validated; errors fail closed.
- [ ] Caller-credential passthrough refuses to start on a non-loopback bind.
- [ ] Provider credentials and prompt bodies not logged by default.
- [ ] Hard per-request cost mode rejects unbounded or unknown billable dimensions.
- [ ] Hard cost mode is enabled only for models whose tokens-≤-bytes bound and reasoning-token billing are verified.
- [ ] Only allowlisted fields are rewritten; caller limits are never raised.
- [ ] `wye explain` output matches `serve` decisions for the same request, credential, and config.
- [ ] Token replay documented and not represented as preventable without redemption state.
- [ ] No global/monthly budget or global RPM enforcement advertised.

### Operations / DX

- [ ] `wye serve --config wye.toml` works from a prebuilt binary.
- [ ] OCI image and Compose example need no gateway datastore.
- [ ] Helm release exposes Wye internally with health probes and secret references.
- [ ] An SDK backend switches using base URL plus gateway credential.
- [ ] `wye claude` and `wye codex` launch their agents against the gateway without changing the user's own configuration.
- [ ] An OTel backend shows usage and cost by subject, model, and session without a custom dashboard.
- [ ] Rollout and shutdown drain streams within a configured deadline.

### Benchmarks

- [ ] Mock upstream varies TTFT, token rate, failure rate, and stream length.
- [ ] Reports include p50/p95/p99 added latency, RSS, CPU, and slow-consumer behavior.
- [ ] Competitor tests use comparable features and disclose differences.
- [ ] Cost-saving claims are backed by usage, pricing, and task-quality evidence.
