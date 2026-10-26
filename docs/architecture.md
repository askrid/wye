# Architecture

One process per replica, passing each client's native API through to a provider that serves it: the Anthropic Messages API for Claude Code and Anthropic SDK clients today, the OpenAI APIs for Codex and OpenAI SDK clients later. Wye never translates between provider APIs ([product.md](product.md#non-goals)).

## System view

```text
                         Git repository
                           wye.toml
                              │
                   CI checks + GitOps rollout
                              │
            deploy same policy version to replicas
                              │
App / Claude Code / Codex ──> ingress / Kubernetes Service
                              │
                    ┌─────────┴─────────┐
                    ▼                   ▼
              Wye replica A       Wye replica B
                    │                   │
            authenticate        authenticate
            request policy      request policy
            route / proxy       route / proxy
            stream / cancel     stream / cancel
                    └─────────┬─────────┘
                              ▼
                        LLM providers

              Each Wye replica ──OTLP──> existing telemetry
```

Replicas never communicate at request time. A new request may land on any ready replica. A stream stays on its replica until completion; active streams are not migrated.

Infrastructure provides service discovery, TLS/ingress, scheduling, replica count, rollout, secret delivery, and any aggregate rate limiting. Wye provides its listener, graceful shutdown, and health endpoints ([operations.md](operations.md)).

## Request path (target)

1. Validate endpoint, framing, and request size.
2. Verify the signed gateway credential against a configured verification key.
3. Enforce static model permissions and per-request limits ([policy.md](policy.md)).
4. Resolve a model alias; select the route that serves this endpoint natively, and among its upstreams the one policy picks (`cache_affine`).
5. Apply allowlisted rewrites (the resolved model, the provider's model id, agent-aware model routing).
6. Forward upstream unchanged beyond the allowlist.
7. Stream incrementally with downstream backpressure; preserve tool calls and event semantics.
8. Propagate cancellation, apply the retry policy, release request resources.
9. Extract reported usage, or estimate it or mark it unknown; export telemetry asynchronously.

**Implemented** for Anthropic Messages: steps 1, 2, 3 (model permissions and output-token ceilings; cost ceilings are not built), 6–8, and 9 as usage records in a JSON-lines log and as OTLP logs, metrics, and spans. Not built: cost ceilings in step 3, and steps 4–5 (one upstream and one account today).

## Repository and workspace

One repository, four Cargo workspace members:

```text
wye/
├── crates/
│   ├── gateway-core/src/        portable policy, no I/O
│   │   ├── config.rs            wye.toml parsing and validation (config/validate/)
│   │   ├── credential.rs        gateway credential verification (own and identity-provider) and issuance
│   │   ├── credential/keys.rs   Ed25519, ES256, RS256 keys; JWKS parsing
│   │   ├── credential/provider.rs  identity-provider claims and grants
│   │   ├── policy.rs            model permissions, output-token ceilings, request-body scanner
│   │   ├── prompt.rs            prompt hashes for cache-miss diagnostics
│   │   ├── agent.rs             Claude Code agent and hint headers, bounded and validated
│   │   ├── json.rs              byte-level JSON lexing shared by the scanners
│   │   └── usage.rs             SSE usage and tool-call scanner
│   ├── wye/src/                 the binary
│   │   ├── main.rs              command-line entry
│   │   ├── startup.rs           secrets and files the config references
│   │   ├── server.rs            listener, connections, two-phase shutdown
│   │   ├── server/write_timeout.rs  closes connections whose writes stall
│   │   ├── gateway.rs           routing, admission, gateway errors
│   │   ├── auth.rs              credential checks, cached per connection
│   │   ├── permissions.rs       model permissions and token ceilings on the request body
│   │   ├── discovery.rs         GET /v1/models
│   │   ├── diagnostics.rs       prompt hashes of forwarded request bodies
│   │   ├── upstream.rs          forwarding, TLS client
│   │   ├── upstream/pool.rs     upstream connection lanes
│   │   ├── upstream/body.rs     response body: idle timeout, connection slot
│   │   ├── usage_log.rs         per-request records, bounded writer
│   │   ├── usage_log/           JSON line, OTLP logs, metrics, and traces
│   │   ├── cli/                 explain, policy-test, report, claude, probe, token
│   │   ├── gauge.rs             in-flight counters released on drop
│   │   └── alloc_stats.rs       allocation counters (feature alloc-stats, benchmarks only)
│   ├── wye-testkit/             test CA, mock upstream, test signing keys and IdP tokens
│   └── wye-bench/               benchmark, regression gate, load and capacity runs, competitor runs
├── bench/baseline.json
├── scripts/                     check.sh (commit gate), ab.sh, image-smoke.sh, upload-latency.sh
├── Dockerfile
└── examples/                    wye.toml, subscription.toml, policy-tests/
```

Create modules only when their behavior is implemented.

- `gateway-core` owns configuration types, validation, credential and request policy, the request and response scanners, and later routing. No Tokio, socket, filesystem, or environment dependencies, so it can later back a WASM adapter.
- `wye` owns the CLI, configuration loading, secret resolution, HTTP server and client, telemetry, and shutdown.
- `wye-testkit` and `wye-bench` are development-only; see [development.md](development.md).

Core, runtime, tests, and deployment artifacts change together and need coordinated review and release; multiple repositories would add versioning work without an ownership boundary. Do not add provider crates or a speculative WASM transport interface.

## Runtime stack

| Concern | Technology |
|---|---|
| Async execution | Tokio |
| HTTP | Hyper 1.x, `hyper-util`, `http-body-util` |
| TLS | Rustls (`ring` provider) upstream, built-in web PKI roots plus `upstream.ca_file`; deployment terminates inbound TLS |
| Credentials | `ring` Ed25519, ECDSA P-256, and RSA verification, `base64` |
| Hashing | `xxhash-rust` xxh3 (prompt hashes), `ring` HMAC-SHA256 (their exported tags) and SHA-256 (session trace ids) |
| Config / JSON | Serde, `toml`, `serde_json`; hand-written streaming scanners for request bodies and SSE (`memchr`) |
| Buffers | `bytes` and streaming bodies |
| Instrumentation | Usage records as a JSON-lines log on stderr and as OTLP logs, OTLP metrics, and OTLP spans (hand-written OTLP/HTTP JSON exporter on the existing hyper client, instead of the OpenTelemetry Rust SDK and its gRPC stack) |
| Edge | Separate WASM adapter later |

Direct Hyper gives explicit control over request and response bodies. Reconsider Axum if routing and middleware grow enough to justify it.

A request may run tens of seconds with little CPU. The important costs are per-connection buffers, HTTP/2 multiplexing, TLS, memory per task, fairness, slow-client backpressure, and cancellation. Never block executor threads with synchronous work.

Upstream connections come from one pool of lanes ([upstream connections](#upstream-connections)). Immutable configuration, connection pools, and in-flight request state are compatible with statelessness; persistent conversations, global quotas, and cumulative spend accounting are not.

## Public API

Implemented:

- `POST /v1/messages`, `POST /v1/messages/count_tokens`: Anthropic Messages, native passthrough to Anthropic (P0: Claude Code and Anthropic SDK clients).
- `GET /healthz`, `GET /readyz`: operational probes; `HEAD /api/hello`: Claude Code's warm-up probe.
- `GET /v1/models`: with `[discovery]`, the configured catalog narrowed to the models the caller may use (the `[auth] models` ceiling, then the credential's `models` claim), so Claude Code's model picker shows only permitted models. Authenticated when `[auth]` is set; one page (`has_more: false`); no upstream contact and no usage record. Claude Code queries it only with `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`, with a 3-second timeout, and keeps ids containing `claude` or `anthropic`.

Planned (M3): `POST /v1/responses` and `POST /v1/chat/completions`, native passthrough to OpenAI. Every other path gets `404 not_found_error`.

An `ANTHROPIC_BASE_URL` or `OPENAI_BASE_URL` override works only when the client respects it **and** uses an implemented endpoint. Base-URL compatibility is not compatibility with proprietary SDK extensions.

### Claude Code gateway contract

Claude Code documents what a gateway must do ([gateway compatibility guide](https://code.claude.com/docs/en/llm-gateway-protocol)). P0 acceptance tests encode it:

- Match on path, not full URL: inference posts to `/v1/messages?beta=true`; forward the query string.
- Forward all `anthropic-*` request headers and all body fields unchanged, as open lists. Never allowlist `anthropic-beta` values: capabilities pair a beta header with a body field, and stripping one half produces `400`s on the next Claude Code release.
- Forward `retry-after`, `x-should-retry`, and `anthropic-ratelimit-unified-*` response headers, and upstream error bodies unmodified; Claude Code's retry and capability-recovery logic matches upstream error wording.
- Relay every SSE event, including `ping`. Claude Code aborts an idle stream and treats a stream that ends before `message_delta`/`message_stop` as dropped.
- Forward the `system` array unchanged; its first block is an attribution block the Anthropic API strips by position.
- Consume, never require, `x-claude-code-session-id`, `x-claude-code-agent-id`, `x-claude-code-parent-agent-id`, and the gateway hint headers (`x-claude-code-request-class`, `-agent-type`, `-compaction`, `-context-compacted`, `-prompt-id`, `-prev-tool-durations`) for telemetry attribution, [agent-native visibility](research.md#agent-native-directions), and, later, [agent-aware routing](research.md#1-agent-aware-routing). A custom base URL receives hint headers only when developers set `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`. Values are client-supplied: bound and validate them before they become telemetry attributes, and never let them authorize anything.
- Answer the best-effort `HEAD /api/hello` warm-up probe with an empty `200` before authentication; it carries no data and reaches no upstream.
- `/v1/messages/count_tokens` is unbilled: it requires authentication and model permission but skips cost admission.
- Gateway-generated errors use Anthropic's error JSON shape and status codes, so Claude Code's retry and recovery logic reads them as it reads upstream errors. Wye's own `529 overloaded_error` (concurrency or held-bytes limit, always before any upstream contact) carries `retry-after: 1` and `x-should-retry: true`: integer seconds at most 60, or Claude Code stops retrying.
- Gateway sessions are not eligible for Claude Code's HIPAA configuration; regulated teams must know before adopting Wye.
- Claude Code sends background requests to a second, smaller model unless one is pinned; model permissions must cover it or those requests fail.

The guide changes with the client. Re-run the contract tests against each targeted Claude Code release.

## Forwarding contract

Each endpoint forwards to the corresponding upstream endpoint without converting between APIs.

1. Validate the endpoint and bounded request size.
2. Construct the configured upstream URL and headers.
3. Send through the shared client.
4. Forward upstream status and permitted headers.
5. Stream the response body as the downstream consumes it.

**Bodies.** Preserve request JSON, unknown fields, tool definitions, and provider-native options. The request body streams to the upstream unchanged, bounded by `server.max_request_body_bytes` (default 32 MiB, Anthropic's limit). With [model permissions or output-token ceilings](policy.md#gateway-credentials), only the start of the body is held, until its top-level `model` has been read; the held bytes are bounded across the process by `server.max_held_request_bytes`, and the body is never re-serialized. Never buffer the generated response. Relay SSE bytes without parsing or rebuilding events; HTTP chunks are not SSE event boundaries. Preserve non-streaming JSON and upstream error bodies.

**Request rewrites.** Wye rewrites only an explicit allowlist of request fields; everything else passes through byte-equivalent in meaning:

| Field | When | Why |
|---|---|---|
| `model` | [Model alias](#model-aliases-and-upgrade-checks) (M2b); agent-aware routing and a Claude provider's model id (M4) | Replace an alias with the model config maps it to, a model policy selects for the request class, or the selected provider's id for the same model (`anthropic.claude-opus-5-5` on Bedrock). Never a different model family |
| `model` → URL, `anthropic_version` | Vertex AI (M4) | Vertex names the model in the URL and needs `"anthropic_version": "vertex-2023-10-16"` in the body: the top-level `model` member is cut and the version member spliced in; every other byte is unchanged |
| `max_tokens` / `max_completion_tokens` / `max_output_tokens` | Hard cost mode | Insert the output bound used in the [cost ceiling](policy.md#provable-cost-ceiling) when absent; lower a caller value only when the route explicitly accepts truncation, otherwise reject; never raise a caller value. On Messages, never lower below `thinking.budget_tokens`; reject instead |
| `stream_options.include_usage` | Streaming Chat Completions with usage accounting | OpenAI omits streamed usage otherwise ([usage extraction](#usage-extraction)) |

Each rewrite is per-route configuration, covered by tests, and reported by `wye explain`. Adding a field to this list is a design change.

### Usage extraction

A bounded, read-only observer reads the same bytes Wye relays and recognizes only usage-bearing events; it never delays, buffers, or changes them, and an oversized or malformed event marks usage `unknown` instead of failing the stream.

For Anthropic Messages streams (`gateway-core/src/usage.rs`): at most one partial line is kept (32 bytes for ordinary events, 64 KiB for usage events), line ends are found with SIMD `memchr`, and JSON is parsed only for `message_start` and `message_delta` (usage) and `content_block_start` (tool names, borrowed). A body wrapper emits one record when the body is dropped, so completed, failed, cancelled, and gateway-error outcomes are all recorded; completion is also detected through `is_end_stream`, because hyper stops polling a known-length body before its end. Non-streaming responses are recorded `not_observed`. Messages streams always carry usage, so no rewrite is needed. OpenAI Chat Completions streams carry it only with `stream_options.include_usage`, which adds a final chunk with empty `choices` that some clients mishandle; the route decides whether to inject it (M3).

**Headers and destination.** Strip hop-by-hop headers, including those named by `Connection`. Replace caller authorization (`Authorization` or `x-api-key`) with the configured provider credential, except in explicit loopback [caller-credential passthrough](policy.md#caller-credential-passthrough). Forward only appropriate end-to-end headers. Replace `accept-encoding` with `identity`: Claude Code asks for Brotli and Anthropic complies, which would hide usage from the [observer](#usage-extraction); an uncompressed response costs only bytes, decompressing a copy costs CPU per event ([research.md](research.md#current-harness-and-baseline)). The caller never selects the upstream destination (no open proxy or SSRF surface). Preserve the configured base-path prefix.

**Backpressure and cancellation.** Downstream body polling applies backpressure. No unbounded queues or detached readers. Client disconnect releases or cancels upstream work, verified by integration tests. Already generated tokens may still be billed.

**Errors.** Before downstream headers are sent, local failures produce a gateway error in the client-facing API's error shape (Anthropic error JSON on `/v1/messages`). Upstream error bodies pass through unmodified. After headers are sent, an upstream failure terminates the stream; it cannot become a new JSON error response.

**Timeouts and shutdown.** Use three timeouts: a short connect timeout; a response-header timeout of several minutes, because a non-streaming generation sends no headers until it completes (Claude Code falls back to non-streaming requests after a stream failure); and a streaming-inactivity timeout longer than the provider's `ping` interval. A fourth bounds the client side: a connection whose response writes make no progress for `client_write_timeout_secs` is closed, cancelling its upstream request, because a client that stops reading leaves its response unpolled and the inactivity timer never runs. No total request timeout: it would kill valid generations. Shutdown has a drain phase (readiness fails, requests still served) and a grace phase that cancels whatever remains ([operations.md](operations.md#health-and-shutdown)).

## Retries and fallback

The initial proxy does not retry or fall back: an ambiguous failure may already have incurred charges, and switching providers after output starts cannot preserve the response. On `/v1/messages`, Wye does not retry the same provider: Claude Code retries on its own, guided by the `x-should-retry` and `retry-after` headers Wye forwards, and gateway retries would multiply attempts and risk duplicate billing. The one planned exception is [failover to another Claude provider](#claude-across-providers-planned-m4) after a refusal that proves the request was not processed (`429`, `529`, connection refused), before any response byte was sent. When retries are added for other routes, they follow this policy:

| Failure stage | Policy |
|---|---|
| Local validation, before upstream send | No upstream work; return error or choose another eligible route |
| Connect failure before the request reaches the provider | Retry may be safe within a bounded deadline |
| Upstream may have accepted the request, no response yet | Retry risks duplicate billing; conservative |
| Output already sent downstream | Never transparently replay from another model |
| Partial tool-call payload emitted | No automatic fallback without higher-level recovery semantics |

Keep request IDs and trace linkage. Do not claim exactly-once execution.

## Providers

A provider is a concrete module for one way of reaching a native API: Anthropic today; Claude Platform on AWS, Bedrock, and Vertex for the same Messages API (M4); OpenAI for `/v1/responses` and `/v1/chat/completions` (M3). A closed `Provider` enum selects it when the second one lands; there is no trait with one implementation and no translation layer. What differs per provider is only what Wye reads and replaces: endpoint and credential, model id, the usage events (for the usage observer and cost), and the error shape of gateway-generated responses. A request for an endpoint no configured provider serves is rejected, never converted. Passing provider-native options through does not mean Wye interprets or enforces them.

### Upstream connections

hyper-util's client pool keeps a single HTTP/2 connection per host and never opens another, so requests beyond the provider's concurrent-stream limit would queue inside it. `upstream/pool.rs` keeps 64 lanes, each a client with its own pool and therefore its own connection, created at startup and connected lazily. Each request takes a slot on the least loaded lane (ties to the earliest), held by the response body until the stream ends: sequential traffic reuses one connection, `k` concurrent requests use up to `k` connections, and beyond 64 the load stays even. Spreading is deliberate: `h2` guards all streams of one connection with one mutex, and packing streams onto a connection made that lock the dominant cost of the TLS path. Streams on a lane also share its HTTP/2 connection window; with hyper's default (5 MiB, 2 MiB per stream), three clients that stop reading filled it and stalled every other stream on the lane. The connection window is therefore the protocol maximum, and buffering is bounded per stream instead (256 KiB window, measured 0.56 MB of heap): a stalled client holds only its own stream, and only until `client_write_timeout_secs`. The worst case, every in-flight request stalled at once, is `max_concurrent_requests` × 0.56 MB.

### Upstream-held state

Some requests reference state stored by a specific provider account: Responses `previous_response_id` (with `store=true`), stored conversations, uploaded file IDs, and provider cache entries. Wye stores none of it, but routing must respect it:

- A request that references upstream-held state is pinned to the provider and credential that can resolve it, or rejected with an explicit error. It is never routed elsewhere.
- Determining that provider and credential must be stateless: from the route configuration (a route with a single eligible account) or the gateway credential. If neither identifies it, reject.

### Cache-affine routing

Provider prompt caches are per account (and sometimes per region), and cached input is priced at a large discount. Cost routing that spreads one conversation across accounts or providers forfeits that discount.

When a route has several eligible candidates, Wye chooses deterministically by rendezvous hashing over them, keyed by the Claude Code session id when the request carries one, and otherwise by the credential subject plus a hash of a stable request prefix (system prompt and tool definitions). The same workload then reaches the same candidate from any replica, with no shared state, and moves only when the candidate set changes. Strategy name: `cache_affine` (planned for [M4](#claude-across-providers-planned-m4)). Its cost benefit is a hypothesis to measure ([research.md](research.md#questions-to-test)).

Pure affinity can overload one candidate when a few prefixes dominate traffic, a capacity loss recent serving research reports ([research.md](research.md#2-cache-miss-diagnostics)). The strategy therefore needs a load bound that stays stateless: a static share per candidate in config (consistent hashing with bounded loads applied to configured weights), or a placement plan fitted offline from telemetry and committed to config. Every replica still computes the same choice.

### Model aliases and upgrade checks

**Implemented: aliases (M2b stage A).** A client asks for a stable name and config decides the model:

```toml
[[aliases]]
name = "coding-default"
model = "claude-opus-5-5"
# Staged rollout: credentials whose subject matches get another model first.
when = [{ subjects = ["team-canary-*"], model = "claude-sonnet-5-5" }]
```

- **Resolution** reads the top-level `model` with the existing [policy scanner](policy.md#gateway-credentials) hold (Claude Code and the SDKs send `model` first; only `[[aliases]]` being configured makes the hold apply to every request) and splices the resolved name in place of the alias: the held chunks are sliced around the value, so only the new name is allocated, every other byte is unchanged, and `content-length` is adjusted by the difference. Names that are not aliases pass through untouched, so aliases are opt-in per request.
- **Rollout:** `when` rules match the credential's subject (exact or a prefix ending in `*`, as in model permissions); the first match wins, and callers without a credential get the alias's `model`. Resolution is one step: an alias names a model, never another alias.
- **Policy on the model that runs:** model permissions (and pricing, M2) apply to the resolved model. `GET /v1/models` lists aliases after the catalog's models, each when the model it resolves to for that caller is permitted. Claude Code's model picker keeps only ids containing `claude` or `anthropic`, so aliases meant for it are named accordingly (`claude-coding`).
- **Visible:** usage records carry `requested_model` (the alias) next to `model` (as the provider reports it), OTLP logs `wye.requested_model`; `wye explain` prints the resolution, and a policy test can pin the resolved model (`model = "claude-opus-5-5"` in its `.expect.toml`), so a rollout is reviewed like any policy change.
- **Cost**, measured with every benchmark request aliased: one allocation per request for the hold (as with model permissions), one more when the name's length changes (the new `content-length`), CPU per request within noise. With aliases alone the rest of the body is not lexed: the repeated-`model` check protects permissions, which do not apply (Claude Code-shaped uploads 1.64 vs 1.49 ms per MB, within noise; with the check, 2.16).
- An alias names one model. Mapping one name to several providers of that model is [M4](#claude-across-providers-planned-m4)'s route, not a second alias mechanism.

**Implemented: upgrade checks (M2b stage B).** Moving an alias, or a client, to a new model fails in production when the new model rejects what the old one accepted. Wye ships a rule table of each model's breaking request changes, every rule citing the provider's migration notes and covered by its own test (`gateway-core/src/upgrade.rs`):

| Rule | Request field | Rejected by (dated ids included) |
|---|---|---|
| `thinking_disabled` | `thinking.type` `disabled` | Opus 5.5 |
| `budget_tokens` | `thinking.type` `enabled` (manual budget) | Opus 4.7, 4.8, 5, 5.5; Sonnet 5, 5.5; Fable 5, 5.1 |
| `forced_tool_choice` | `tool_choice.type` `any` or `tool` | Opus 5.5 |
| `computer_tool` | `tools[].type` `computer_20250124` or `computer_20251124` | Opus 5.5 (Claude API) |
| `prefill` | the last `messages[]` entry is an assistant turn | Opus 4.6 and later; Sonnet 5 |
| `sampling` | `temperature`, `top_p`, or `top_k` | Sonnet 5; Opus 5.5 |
| `system_role` | a `messages[]` entry with role `system` | Opus 4.7 |

Source: Anthropic's [Opus 5.5 migration guide](https://platform.claude.com/docs/en/models/opus-5-5/migration-guide), read 2026-10-26, and its API reference for Sonnet 5.5 and Fable (`budget_tokens`). The deprecated `output_format` is rejected without a beta header on every model, so it is not an upgrade rule.

```toml
[upgrade_check]
targets = ["claude-opus-5-5"]          # models an alias or a client may move to; up to 16
sample_percent = 10                    # share of sessions (or, without one, clients) checked; default 100
```

- **Scan:** for sampled requests, a constant-memory structure scanner on the forwarded body reads only the fields the rules name (keys at the first three levels; `thinking.type`, `tool_choice.type`, `tools[].type`, `messages[].role`) and records which rules each target breaks. Sampling is by Claude Code session id, or by the credential's subject for SDK callers without one, alike on every replica. Cost: on a Claude Code-shaped 1 MiB body it scans in 0.77× the time of the model-permission scan and half that of cache diagnostics (same machine, same body), so about 0.5 ms per MB in the gateway.
- **Records:** `upgrade_issues` in the usage record (`{"claude-opus-5-5": ["budget_tokens", "prefill"]}`; `{}` for a checked request no target rejects), `wye.upgrade_issues` in OTLP logs, and the `wye.upgrade.issues` metric by target, rule, and subject. The backend shows which clients would fail before the switch.
- **Offline:** `wye explain --target-model claude-opus-5-5 request.json` (repeatable; without it, the configured targets) prints each issue with the fix and its source; `wye report` lists issues by target, rule, and client.
- Report only: the request is forwarded unchanged ([non-goal](product.md#non-goals)). The rule table is versioned with Wye.

### Claude across providers (planned, M4)

The same Claude models are sold by four providers. Wye can serve one Claude Code fleet from several of them, choosing per request and failing over, because all four speak the Anthropic Messages API with small, known differences. This is muxing, not translation: no request goes to another model family.

| Provider | Endpoint | Request | Stream | Auth | Model id |
|---|---|---|---|---|---|
| Anthropic | `api.anthropic.com/v1/messages` | Messages | SSE | `x-api-key` | `claude-opus-5-5` |
| Claude Platform on AWS (Anthropic-operated) | `aws-external-anthropic.{region}.api.aws/v1/messages` | Messages, same-day parity | SSE | SigV4 (`aws-external-anthropic`) or short-term API key; workspace id required | `claude-opus-5-5` |
| Amazon Bedrock, Messages endpoint (Opus 4.7 and later) | `bedrock-mantle.{region}.api.aws/anthropic/v1/messages` | Messages | SSE | SigV4 (`bedrock-mantle`) or a Bedrock API key in `x-api-key` | `anthropic.claude-opus-5-5` |
| Google Vertex AI | `aiplatform.googleapis.com/v1/projects/{p}/locations/{loc}/publishers/anthropic/models/{model}:streamRawPredict` | Messages without `model`, with `anthropic_version` | SSE (to verify) | OAuth access token (`Authorization: Bearer`) | `claude-opus-5-5` |

Sources: Anthropic's platform pages for Bedrock, Vertex, and Claude Platform on AWS (read 2026-10-22). The legacy Bedrock `InvokeModel` path streams AWS event-stream frames, not SSE, so it would need re-framing per event and is excluded. Partner platforms lack some first-party features (Files API, URL sources, some server tools, `fallbacks`); a request that uses one fails at that provider with the provider's own error, and Wye does not hide the difference.

**Selection.** Candidates are configured per route (`[[routes.<name>.upstreams]]`, all serving the same models). Wye picks deterministically, from any replica alike: [`cache_affine`](#cache-affine-routing) by session id (prompt caches are per provider and account, so a session stays on one), weighted by configured shares, with `ordered` as the simple alternative (first candidate unless it is excluded by the request's model or a feature it is known not to support). No health state is shared or remembered between requests.

**Failover within a request**, only where it cannot duplicate billing: when a candidate answers `429`, `529`, or a connection refusal before any response byte reached the client, Wye sends the same request to the next candidate. A `5xx` after the provider may have accepted the request is not retried ([retries](#retries-and-fallback)). Resending needs the request body, so a route with failover holds the body up to `failover_max_body_bytes`, counted against `server.max_held_request_bytes`; larger bodies are forwarded without failover. Claude Code still retries on its own, so failover is a latency and availability improvement, not a correctness requirement. A failover costs that turn its prompt-cache discount at the new provider; affinity brings the next turn back.

**Auth without state.** Each provider credential is resolved at startup like `api_key_env`. SigV4 signs each request on the request path (an HMAC chain, microseconds) with an unsigned payload hash if the service accepts it, since hashing a streamed multi-MB body before sending would mean buffering it; if not, Bedrock and AWS API keys avoid signing. Vertex access tokens expire after about an hour: either Wye exchanges a service-account key for tokens itself (an RS256-signed JWT and one HTTPS call per hour, cached in memory per process), or an external refresher writes a token file Wye re-reads. Both are open decisions below.

**Telemetry and cost.** Usage records and OTLP gain `wye.provider` and the account id; pricing entries can be scoped to a provider, since partner and regional prices differ ([policy.md](policy.md#pricing-metadata)).

### Tools and MCP

```text
Agent ──tools/schema──> Wye ──> model
Agent <─tool_calls───── Wye <── model
Agent ──executes tool / uses MCP server independently
Agent ──tool results─> Wye ──> model
```

This pattern needs no MCP gateway. Provider-hosted tools pass through like any other field; Wye neither runs nor rewrites them.


## Configuration

One TOML file, loaded at startup, is the source of truth for aliases, providers, routes, credential verification, and limits. Secrets are referenced by environment variable, never stored in Git, images, or IaC state.

The runtime resolves environment variables; the core validates configuration without resolving secrets. Reject unknown fields, invalid URLs or limits, broken references, duplicate IDs, unsupported provider/API pairs, missing credentials, and a policy that needs prices without them at startup. Never print secrets or log request bodies by default.

No hot reload; changes require a restart (or rolling deploy). A config snapshot stays consistent within a request. Planned: include the config version in logs and traces to diagnose rollout skew.

### Current schema (implemented)

Every field except `listen`, `base_url`, and one credential mode is optional; values shown are defaults. `examples/wye.toml` is the annotated version.

```toml
[server]
listen = "127.0.0.1:8080"              # non-loopback only with [auth]
max_request_body_bytes = 33554432
max_held_request_bytes = 67108864       # read for model permissions, all requests together
max_concurrent_requests = 10000        # beyond: 529 overloaded_error
drain_delay_secs = 0                   # /readyz fails, still serving
shutdown_grace_secs = 30               # then remaining requests are cancelled
client_write_timeout_secs = 60         # a client accepting no response bytes this long is disconnected

[upstream]
base_url = "https://api.anthropic.com" # http only for loopback hosts
api_key_env = "ANTHROPIC_API_KEY"      # or: api_key_file = "/run/secrets/anthropic" (relative to this file)
                                       # or: forward_caller_credential = true (loopback, no [auth])
# ca_file = "corporate-ca.pem"         # extra trusted roots, relative to this file
connect_timeout_secs = 10
response_header_timeout_secs = 600
stream_idle_timeout_secs = 120
http_version = "auto"                  # HTTP/2 via ALPN when offered; "1.1": no HTTP/2 flow control on uploads

[auth]                                 # optional
issuer = "wye"
audience = "wye-prod"
max_credential_ttl_secs = 604800
# models = ["claude-sonnet-*", "claude-haiku-*"]  # ceiling; a credential's models claim narrows it
# max_output_tokens = 32000            # max_tokens ceiling; a credential's claim can only lower it
[[auth.keys]]                          # after the [auth] fields: TOML assigns what follows to the key
id = "k1"
ed25519_public_key = "<from wye token keygen>"
[[auth.issuers]]                       # optional: an identity provider; issuer, audience, keys above become optional
issuer = "https://token.actions.githubusercontent.com"
audience = "wye-prod"
algorithm = "RS256"                    # or ES256, EdDSA; pinned, never read from the token
jwks_file = "github.jwks"              # the provider's JWKS, relative to this file, read at startup
subject_claim = "sub"                  # names the usage-log subject
[[auth.issuers.grants]]                # optional: models by claim value; none matching = no model
claim = "repository_owner"
value = "acme"
models = ["claude-haiku-*"]

[telemetry]                            # optional: OTLP logs, metrics, and traces
otlp_endpoint = "http://collector:4318" # POST {otlp_endpoint}/v1/{logs,metrics,traces}, OTLP/HTTP JSON
# otlp_headers_env = "OTLP_HEADERS"    # extra headers, "key=value,key2=value2"
service_name = "wye"

[cache_diagnostics]                    # optional: prompt hashes in the usage log
hash_key_env = "WYE_CACHE_HASH_KEY"    # required; at least 32 bytes, same on every replica
sample_percent = 100                   # share of Claude Code sessions diagnosed

[discovery]                            # optional: GET /v1/models; absent without it
models = [                             # up to 1000; listed per caller after model permissions
  { id = "claude-sonnet-5-5", display_name = "Claude Sonnet 5.5", description = "Everyday coding" },
  { id = "claude-haiku-5-5" },         # display_name and description optional
]

[[aliases]]                            # optional, up to 256: a name clients send, the model that runs
name = "coding-default"
model = "claude-opus-5-5"
when = [{ subjects = ["team-canary-*"], model = "claude-sonnet-5-5" }]  # first match by subject; up to 32

[pricing]                              # optional: cost in usage records; rules in policy.md
version = "anthropic-2026-10-06"       # recorded with every cost
[[pricing.models]]                     # up to 256; first match on the reported model wins
models = ["claude-opus-5-5"]
input = "4.00"                         # USD per million tokens, at most 6 decimals
output = "20.00"
cache_read = "0.20"                    # optional, like cache_write_5m and cache_write_1h

[upgrade_check]                        # optional: which requests each target model would reject
targets = ["claude-opus-5-5"]          # up to 16 model names
sample_percent = 100                   # share of sessions (or clients) checked
```

### Target schema (illustrative, not implemented)

Extends the current schema; `[server]`, `[auth]`, `[telemetry]`, `[cache_diagnostics]`, `[discovery]`, `[[aliases]]`, `[pricing]`, and `[upgrade_check]` keep their fields, and `[upstream]` stays valid as the single-Anthropic-upstream shorthand.

```toml
# A route serves one native API; a request goes to the route for its endpoint.
[routes.claude]
api = "anthropic-messages"             # /v1/messages, /v1/messages/count_tokens
select = "cache_affine"                # by session id; stateless, same choice on every replica
failover = ["429", "529", "connect"]   # before any response byte; never on ambiguous 5xx
failover_max_body_bytes = 8388608      # bodies held for resending, within max_held_request_bytes

[[routes.claude.upstreams]]
id = "anthropic-team-a"
provider = "anthropic"
api_key_env = "ANTHROPIC_KEY_TEAM_A"
weight = 2

[[routes.claude.upstreams]]
id = "bedrock-us"
provider = "bedrock"                   # Messages endpoint; model ids get the anthropic. prefix
region = "us-east-1"
api_key_env = "BEDROCK_API_KEY"        # or aws_credentials = "environment" for SigV4

[[routes.claude.upstreams]]
id = "vertex-global"
provider = "vertex"
project = "acme-ml"
location = "global"
service_account_file = "vertex-sa.json"  # open decision: token exchange in Wye, or token_file

[routes.openai]
api = "openai"                         # /v1/responses, /v1/chat/completions (M3)
[[routes.openai.upstreams]]
id = "openai"
provider = "openai"
api_key_env = "OPENAI_API_KEY"

# Agent-aware model routing within a provider: the allowlisted `model` rewrite.
[[routing.rules]]
request_class = "auxiliary"            # x-claude-code-request-class
model = "claude-haiku-5-5"

[policy]
max_request_cost_usd = "0.05"          # provable bound; needs [pricing]
unknown_cost_behavior = "reject"
```

## Invariants

1. No full-response buffering for streaming.
2. Bounded memory under a slow downstream consumer.
3. Client cancellation releases gateway resources and attempts upstream cancellation.
4. Tool-call and event streams preserve ordering and identifiers for supported pairs.
5. Every upstream retry follows an explicit safety and deadline policy.
6. A configuration snapshot is consistent within a request.
7. Authentication and hard constraints are checked before upstream work.
8. No request depends on cross-request quota or history.
9. Only allowlisted request fields are rewritten; a caller's limit is never raised.
10. Usage observation never alters, delays, or buffers forwarded bytes.
11. Requests referencing upstream-held state reach the account that holds it, or are rejected.
12. Routing is a pure function of request, credential, and config, so `wye explain` and replicas always agree.

## Open design decisions

Settled decisions live in the sections above; this table holds only what is still open.

| Topic | Current preference / unresolved point |
|---|---|
| `include_usage` injection | Per-route; default undecided until client compatibility is tested |
| Configuration reload | Startup snapshot first; hot reload only on demand and without disrupting streams |
| SigV4 for AWS providers | Sign per request with an unsigned payload hash if Bedrock and Claude Platform on AWS accept it (to verify); otherwise support only their API keys |
| Vertex access tokens | Token exchange inside Wye (service-account key, RS256 JWT, one call per hour, in-memory cache) or an external refresher writing a token file; undecided |
| Failover body hold | Default bound for `failover_max_body_bytes`; agent contexts reach several MB, so the bound trades failover coverage against memory |
| Edge support | WASM adapter after native correctness |
| License / package name | After package, domain, and trademark checks |
