# Roadmap

Each stage leaves a runnable system with focused tests. Provider phases (P0–P4) are defined in [product.md](product.md#provider-and-endpoint-phases).

## Status

| Milestone | State |
|---|---|
| M0: Claude Code stream proxy | Implemented and gated. Open: the exit check against a real Claude Code session has not been run. |
| Realistic-workload benchmark | Done: uploads, 2,000 token-paced streams, KrakenD and LiteLLM comparisons |
| M1 stage 4a: gateway credentials | Done: Ed25519 JWT credentials, `wye token`, network listen with `[auth]` |
| M1 stage 4b: model permissions | Done: `[auth] models` ceiling narrowed by a credential's `models` claim, checked before upstream contact |
| M1 stages 5–6 | Usage scanner, OTLP logs, metrics, and traces, `upstream.api_key_file`, `wye probe`, and `wye claude` done, and an OCI image with a smoke test; published images and binaries, and Compose not started |
| Cache-miss diagnostics (from M2) | Done, opt-in: keyed prompt hashes in the usage log, with per-session sampling to bound CPU (structure-dense bodies cost more than the target per diagnosed request); `diag_` metrics gated |
| M1b: agent-aware visibility | A1 done (retry headers on Wye's 529s, `GET /v1/models` from policy); A2 done (agent headers in usage records and OTLP, tokens by request class, cache misses by reason); A3 implemented (session traces, tool spans; the Jaeger check is open); A4–A5 not started. Directions ranked in [research.md](research.md#agent-native-directions) |
| M2–M6 | Not started |

## Milestones

### Realistic-workload benchmark

Bulk throughput is the least relevant metric for an LLM gateway; traffic is token-paced and requests are large. Before further optimization or competitor claims:

- **Large request bodies:** Claude Code resends its whole context every turn (hundreds of KB to several MB). Measure added upload latency and CPU per uploaded MB.
- **Token-paced streams at scale:** 1,000–10,000 concurrent streams at ~50 events/s with ~100-byte events. Measure CPU per streamed event, memory per active stream, and p99 time to first byte and per-event delay under that load. Make CPU per event the primary efficiency metric; keep bulk MB/s as a secondary one.
- **Competitors that stream:** LiteLLM, then Envoy AI Gateway, in feature-equivalent pass-through configurations. KrakenD CE stays as a reference but cannot stream SSE.

**Exit:** gated metrics for all three workloads, and a comparison table against at least one streaming-capable gateway.

**Status: done.** Uploads and token-paced streams are measured and gated (paced on the TLS + HTTP/2 path only). LiteLLM is benchmarked as the streaming-capable competitor ([research.md](research.md#against-litellm)); Envoy AI Gateway remains a later comparison.

### M0: Claude Code stream proxy (P0)

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| 1 | Workspace, `wye serve --config`, startup validation, environment secrets, health endpoints | Valid config starts; invalid config fails without exposing secrets |
| 2 | Anthropic Messages JSON and SSE forwarding (`/v1/messages`, `/v1/messages/count_tokens`); request bounds and [local resource bounds](policy.md#local-resource-bounds), connect/header/inactivity timeouts, backpressure, cancellation, no retries; configured API key or explicit loopback [caller-credential passthrough](policy.md#caller-credential-passthrough) | [Claude Code gateway contract](architecture.md#claude-code-gateway-contract) tests pass; upstream status, headers, and error bodies preserved; SSE bytes, including `ping`, arrive before upstream completion; disconnect releases upstream work; forwarded request is unchanged (no rewrites yet) |
| 3 | Lifecycle hardening and graceful shutdown (**done**: drain phase with failing `/readyz`, grace deadline that cancels remaining connections, `max_concurrent_requests` with 529) | Slow readers stay bounded; failure paths release resources; shutdown drains within a finite deadline |

The first reviewable implementation stops after stages 1–2. Resource bounds and cancellation are part of a correct streaming proxy, not later features; stage 3 broadens their verification and adds deployment lifecycle.

**Exit:** a real Claude Code session, with an API key and with a claude.ai login through passthrough, works through Wye with streaming, tool use, prompt caching, extended thinking, and token counting. Loopback only. No multi-provider claim.

**Status:** stages 1–3 are implemented and covered by integration tests against a mock upstream; the real-session exit check is still open.

### M1: team-ready Claude Code

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| 4 | (**4a done:** Ed25519 JWT credentials, `wye token keygen`/`issue`, `[auth]` lifts the loopback-only restriction, subject in usage log. **4b done:** model permissions from config and the credential's `models` claim, read from the held start of the body; output-token ceilings from config and the `max_output_tokens` claim, the stricter applying.) Remaining in this stage: pinned-issuer IdP JWTs. Ed25519 gateway credentials and pinned-issuer IdP JWTs from `Authorization` or `x-api-key`, `wye token issue`, claim → permission mapping, limit precedence, `max_credential_ttl`, `apiKeyHelper` renewal example | Invalid, expired, wrong-audience, unknown-key, and unauthorized credentials rejected before contacting upstream; credential limits only narrow config; background-model requests covered by permissions |
| 5 | Usage observer (`message_start`, `message_delta`; **done**), minimal OTLP telemetry (**done**: usage records as logs with session and subject attribution, cumulative request, duration, token, and active-request metrics, and one span per request joined to the caller's `traceparent`) with developer and Claude Code session attribution, OCI image (**done** locally with a smoke test; publishing open), baseline benchmark | Usage extracted from split and oversized events without altering forwarded bytes; outcomes and latency observable without logging prompts or secrets; image smoke test and benchmark recorded |
| 6 | `wye claude`, prebuilt binaries, Compose example; `upstream.api_key_file` and `wye probe` for container platforms without Kubernetes ([operations.md](operations.md#without-kubernetes); **both done**, as is a first `wye claude`) | Wrapper launches Claude Code against Wye and cleans up; a Docker Swarm service updates without dropping streams |

Stage 4 is required before any non-loopback deployment.

**Exit:** a team runs Claude Code through a shared Wye with per-developer credentials and sees usage per developer and session in its existing OTel backend.

### M1b: agent-aware visibility

Turns the signals Claude Code sends gateways into attribution and explanations ([product.md](product.md#agent-native-within-the-constraints)). Each item is request-local, adds only header reads to the request path, and passes the commit gate. Order follows the ranking in [research.md](research.md#agent-native-directions):

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| A1 (**done**) | Retry hygiene: Wye's own 529s carry `retry-after` (integer seconds, at most 60) and `x-should-retry: true`. Model picker: `GET /v1/models` lists the models the caller's credential may use, in the shape Claude Code's discovery expects | Integration tests for both; Claude Code's picker shows only permitted models |
| A2 (**done**) | Spend by work type: request class, agent type, compaction trigger, agent and prompt ids in the usage log, OTLP logs, and `wye.tokens`. Expected versus unexpected cache misses from `x-claude-code-context-compacted` | Header values bounded and validated before they become attributes; metric series stay within their limit; gate unchanged |
| A3 (**implemented**; Jaeger check open) | Session traces: trace id derived from the session id; tool durations as tool spans; prompt and agent structure as attributes (no synthetic parent spans) | One Claude Code session appears as one trace in a reference backend (Jaeger) across two replicas |
| A4 | `wye report` over the usage log; tool names and counts from `tool_use` events in the response scanner | `alloc_sse_per_event` stays at zero; report output tested on recorded logs |
| A5 | Report-only secret detection, sampled per session | Measured cost per MB recorded; dependency justified; values never leave the process |

**Exit:** a team sees, in its own OTel backend or with `wye report`, each Claude Code session as a structure (prompts, subagents, compactions, tools) with cost by work type and every unexpected cache miss flagged, with no gateway state and no gated metric worse.

### M2: cost policy for Claude

- Pricing metadata and the [provable cost ceiling](policy.md#provable-cost-ceiling) as a runaway guard for Claude models whose bounds are verified, capped by the context window; everything else labelled estimated-only. Off by default on the Claude Code route.
- [Provider-held budgets](policy.md#provider-held-budgets): identity → Anthropic workspace key mapping and explicit budget-exhausted errors.
- `wye explain` and committed policy tests in CI.
- [Cache-miss diagnostics](research.md#2-cache-miss-diagnostics): per-block prompt hashes in usage telemetry. **Done** (usage log and OTLP logs).
- [Agent-aware routing](research.md#1-agent-aware-routing) for Claude models, once its quality impact per request class is measured.

**Exit:** a team's monthly Claude spend is capped by Anthropic workspace limits with no Wye state, and every request reports a provable or explicitly estimated cost bound next to its actual cost.

### M3: OpenAI APIs and Codex (P1, P2)

| Stage | Scope | Acceptance evidence |
| --- | --- | --- |
| 7 | Chat Completions forwarding to OpenAI through the same transport path; `include_usage` policy | The OpenAI SDK uses Wye with equivalent unary and streaming behavior |
| 8 | Responses forwarding, tool-related event fixtures, `wye codex` | Endpoint-specific fixtures pass, including tool payloads |
| 9 | Upstream-held state pinning for `previous_response_id` | Requests referencing stored responses reach the owning account or are rejected |

**Exit:** a real Codex workflow uses Wye's OpenAI-backed Responses route, including supported streaming and tool behavior. No claims about Anthropic Responses.

### M4: multi-provider routing (P3, P4)

- Anthropic and Gemini Chat Completions adapters with tested streaming and tool calls.
- Alias routing, hard capability filtering, and `cache_affine` selection.
- Provider-native prompt-cache handling where supported.
- Helm chart, minimal GitOps example, `wye doctor`.

**Exit:** one Chat Completions client uses eligible OpenAI, Anthropic, or Gemini backends without rewriting application logic; incompatible features fail explicitly. A small team deploys Wye into an existing Kubernetes cluster and monitors usage in its existing OTel backend. Messages stays Anthropic-backed; Responses stays OpenAI-backed.

### M5: benchmarks and optimization

- Reproducible SSE upstream emulator and load generator. **Started early:** `wye-bench` and the commit gate exist since M0, because speed is a release criterion for every milestone, not only M5 ([research.md](research.md#current-harness-and-baseline)). Already covered: TLS + HTTP/2 upstream, uploads, 2,000 token-paced streams, slow-reader backpressure (integration test), KrakenD and LiteLLM. M5 adds 10k streams, Linux and multi-machine runs, Envoy AI Gateway, and replay of the public TraceLab coding-agent trace (real context sizes, output lengths, and turn gaps) as a workload ([research.md](research.md#related-work)).
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

Core unit tests cover configuration validation and, when implemented, credential and routing policy. A small opt-in real-provider smoke test checks SDK compatibility; it does not replace deterministic protocol tests.

## Release acceptance checklist

### Functional / API

- [ ] Every supported endpoint × provider × feature combination has passing conformance and integration tests.
- [ ] The published support matrix is generated from those test results, not written by hand.
- [ ] P0 Anthropic Messages passes the Claude Code gateway contract tests for the target Claude Code version.
- [ ] P1 Chat Completions unary and streaming work with OpenAI.
- [ ] P2 native OpenAI Responses streaming works with the target Codex version.
- [ ] Anthropic and Gemini adapters preserve supported tool-call IDs, deltas, usage, finish reasons, and errors.
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
- [ ] `wye claude` and `wye codex` configure and clean up supported child tools.
- [ ] An OTel backend shows usage and model/project attribution without a custom dashboard.
- [ ] Rollout and shutdown drain streams within a configured deadline.

### Benchmarks

- [ ] Mock upstream varies TTFT, token rate, failure rate, and stream length.
- [ ] Reports include p50/p95/p99 added latency, RSS, CPU, and slow-consumer behavior.
- [ ] Competitor tests use comparable features and disclose differences.
- [ ] Cost-saving claims are backed by usage, pricing, and task-quality evidence.
