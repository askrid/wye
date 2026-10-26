# Architecture

One process per replica, passing each client's native API through to a provider that serves it: the Anthropic Messages API for Claude Code and Anthropic SDK clients, and the OpenAI Responses API for Codex and OpenAI SDK clients (Chat Completions later). Wye never translates between provider APIs ([product.md](product.md#non-goals)).

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

**Implemented** for Anthropic Messages and OpenAI Responses: steps 1, 2, 3 (model permissions and output-token ceilings; cost ceilings are not built), 4 (aliases, the route per endpoint, `cache_affine` among a route's upstreams), 5 for aliases, 6–8, and 9 as usage records in a JSON-lines log and as OTLP logs, metrics, and spans. Not built: cost ceilings in step 3, provider model ids and agent-aware routing in step 5, and failover in step 8.

## Repository and workspace

One repository, four Cargo workspace members:

```text
wye/
├── crates/
│   ├── gateway-core/src/        portable policy, no I/O
│   │   ├── config.rs            wye.toml parsing and validation (config/validate/)
│   │   ├── alias.rs             model aliases and per-subject rollout
│   │   ├── api.rs               the native APIs and what differs between them
│   │   ├── credential.rs        gateway credential verification (own and identity-provider) and issuance
│   │   ├── credential/keys.rs   Ed25519, ES256, RS256 keys; JWKS parsing
│   │   ├── credential/provider.rs  identity-provider claims and grants
│   │   ├── policy.rs            model permissions, output-token ceilings, request-body scanner
│   │   ├── pricing.rs           price table, exact integer cost
│   │   ├── prompt.rs            prompt hashes for cache-miss diagnostics
│   │   ├── route.rs             the upstream for a request: weighted rendezvous by session
│   │   ├── upgrade.rs           upgrade-check rules and their request-body scanner
│   │   ├── agent.rs             Claude Code agent and hint headers, bounded and validated
│   │   ├── json.rs              byte-level JSON lexing shared by the scanners
│   │   └── usage.rs             SSE usage and tool-call scanners: Messages (usage/raw.rs: the events' shapes) and Responses (usage/responses.rs, its data-line lexer in usage/fields.rs)
│   ├── wye/src/                 the binary
│   │   ├── main.rs              command-line entry
│   │   ├── startup.rs           secrets and files the config references
│   │   ├── server.rs            listener, connections, two-phase shutdown
│   │   ├── server/write_timeout.rs  closes connections whose writes stall
│   │   ├── gateway.rs           routing, forwarding, gateway errors
│   │   ├── gateway/admit.rs     body admission: aliases, permissions, token ceilings
│   │   ├── gateway/forward.rs   a proxied request: permit, admission, sampling, upstream choice
│   │   ├── auth.rs              credential checks, cached per connection
│   │   ├── permissions.rs       model permissions and token ceilings on the request body
│   │   ├── discovery.rs         GET /v1/models
│   │   ├── diagnostics.rs       prompt hashes of forwarded request bodies
│   │   ├── upgrade_check.rs     upgrade-check findings of forwarded request bodies
│   │   ├── upstream.rs          forwarding, TLS client
│   │   ├── upstream/pool.rs     upstream connection lanes
│   │   ├── upstream/body.rs     response body: idle timeout, connection slot
│   │   ├── usage_log.rs         per-request records, bounded writer
│   │   ├── usage_log/           JSON line, OTLP logs, metrics, and traces
│   │   ├── cli/                 explain, policy-test, report, claude and codex, probe, token
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

- `POST /v1/messages`, `POST /v1/messages/count_tokens`: Anthropic Messages, native passthrough to Anthropic (P0: Claude Code and Anthropic SDK clients), when `[upstream]` or `[routes.claude]` is set.
- `POST /v1/chat/completions`: OpenAI Chat Completions, native passthrough to OpenAI (P2: OpenAI SDK clients and OpenAI-compatible agents), on the same `[routes.openai]`.
- `POST /v1/responses`: OpenAI Responses, native passthrough to OpenAI (P1: Codex and OpenAI SDK clients), when `[routes.openai]` is set ([Codex integration](#codex-integration)), with usage and cost in records ([usage extraction](#usage-extraction)).
- `GET /healthz`, `GET /readyz`: operational probes; `HEAD /api/hello`: Claude Code's warm-up probe.
- `GET /v1/models`: with `[discovery]`, the configured catalog narrowed to the models the caller may use (the `[auth] models` ceiling, then the credential's `models` claim), so Claude Code's model picker shows only permitted models. Authenticated when `[auth]` is set; one page (`has_more: false`); no upstream contact and no usage record. Claude Code queries it only with `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`, with a 3-second timeout, and keeps ids containing `claude` or `anthropic`. Codex's catalog request (`GET /v1/models?client_version=...`, its own format) gets `404`, so Codex uses its bundled catalog.

Every other path, and an API whose route is not configured, gets `404 not_found_error`.

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

### Codex integration

What Codex sends a custom provider, from its source (openai/codex at `970111f`, read 2026-10-26) and its gateway guide (learn.chatgpt.com, "Gateway compatibility"):

- A custom `[model_providers.<id>]` with `wire_api = "responses"` gets `POST {base_url}/responses` over HTTP with SSE; its `supports_websockets` defaults to false, so no WebSocket transport. The built-in `openai` provider (also reached through `openai_base_url`) tries WebSockets first and falls back to HTTP; `wye codex` therefore defines its own provider.
- The bearer token comes from the environment variable the provider's `env_key` names, as `Authorization: Bearer`: with `[auth]` a Wye credential, which Wye verifies and replaces with the OpenAI key. Request bodies are zstd-compressed only with a ChatGPT login on the built-in provider, never for an API-key custom provider, so Wye reads plain JSON. Codex serializes `model` first ("gateways may inspect request bodies incrementally"), so the model hold usually reads one chunk.
- Headers for attribution, mapped onto the fields Claude Code's headers fill (**implemented**): `session-id` → `session_id` (and affinity, and the session trace); `thread-id` → `agent_id` and `x-codex-parent-thread-id` → `parent_agent_id` (a thread is one agent's conversation); `x-openai-subagent` → `agent_type`, and `request_class` (`collab_spawn` → `subagent`, `compact` → `compaction`, `review` and `memory_consolidation` → `auxiliary`, unknown → `other`). Codex omits the header only on the main thread, so a request without it is `main` when its `originator` names Codex (`codex_cli_rs`, ...), and unclassified from other clients. `x-codex-turn-metadata` (JSON, with the request kind) is not read: header reads only on the request path. Over HTTP Codex replays the whole input each turn and sends no `previous_response_id` (that is WebSocket-only), so no upstream-held state pins a request yet.
- Retries: `request_max_retries` (default 4) on 5xx and transport errors, never on `429`; `stream_max_retries` (5) for interrupted streams. Wye's overload answer on Responses is therefore `503` with `retry-after`, which Codex and the OpenAI SDKs retry.

Gateway-generated errors on `/v1/responses` use OpenAI's shape: `{"error": {"message", "type", "param", "code"}}`, with `type` `invalid_request_error` or `server_error` and `code` naming Wye's reason (`permission_error`, `authentication_error`, `overloaded_error`, ...).

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
| `stream_options.include_usage` | Streaming Chat Completions with usage accounting (planned, off: see open decisions) | OpenAI omits streamed usage otherwise ([usage extraction](#usage-extraction)); until the default is decided, clients that want usage set it themselves |

Each rewrite is per-route configuration, covered by tests, and reported by `wye explain`. Adding a field to this list is a design change.

### Usage extraction

A bounded, read-only observer reads the same bytes Wye relays and recognizes only usage-bearing events; it never delays, buffers, or changes them, and an oversized or malformed event marks usage `unknown` instead of failing the stream.

For Anthropic Messages streams (`gateway-core/src/usage.rs`): at most one partial line is kept (32 bytes for ordinary events, 64 KiB for usage events), line ends are found with SIMD `memchr`, and JSON is parsed only for `message_start` and `message_delta` (usage) and `content_block_start` (tool names, borrowed). A body wrapper emits one record when the body is dropped, so completed, failed, cancelled, and gateway-error outcomes are all recorded; completion is also detected through `is_end_stream`, because hyper stops polling a known-length body before its end. Messages streams always carry usage, so no rewrite is needed.

For Chat Completions streams (`gateway-core/src/usage/chat.rs`): only `data:` lines, found with `memchr`, and parsed only when a substring search (`memmem`) shows they matter: the first chunk (the model), a chunk with `"usage":{`, or one starting a tool call (`"tool_calls"` with a `"name":"` key, which escaped quotes inside argument strings never match). Usage arrives only in a final chunk the client asks for with `stream_options.include_usage`; Wye does not add the field (the [rewrite](#forwarding-contract) is an open decision: the extra chunk with empty `choices` trips some clients), so a stream without it records `unknown`. `prompt_tokens` include cached tokens and are split like Responses' `input_tokens`.

Non-streaming JSON responses (`gateway-core/src/usage/json.rs`, all three APIs; Chat's tool calls sit in `choices` and are not counted) carry `model` and `usage` as top-level members, after the generated content. The same constant-memory lexer (`usage/fields.rs`) reads the body as it is relayed: it keeps `model`, `service_tier`, and the `usage` object (at most 2 KiB) and counts tool calls among the top-level `content` blocks (Messages `tool_use`) or `output` items (Responses calls), reading only each item's `type` and `name`; text and tool inputs are skipped. A body cut off before `usage` is `unknown`. `count_tokens` and other responses that are not JSON are `not_observed`.

For OpenAI Responses streams (`gateway-core/src/usage/responses.rs`): usage arrives only in the terminal event (`response.completed`, `response.incomplete`, `response.failed`), whose data line carries the whole response, output included, often megabytes. That line is lexed as it streams with constant memory, keeping only `response.model`, `response.service_tier`, and the `response.usage` object (at most 2 KiB); strings inside the output are skipped with `memchr`, so a lookalike `"usage"` in generated text is never read. `response.output_item.added` lines (at most 4 KiB, an item without its content) give tool calls by name, and hosted tools billed per call mark the cost partial. Other events are skipped after their `event:` line. A stream cut before the terminal event has no usage (`unknown`); there is no `partial` here, because OpenAI sends no interim counts. `UsageScanner` picks the scanner per API as an enum, so the per-chunk call is a match.

**Headers and destination.** Strip hop-by-hop headers, including those named by `Connection`. Replace caller authorization (`Authorization` or `x-api-key`) with the configured provider credential, except in explicit loopback [caller-credential passthrough](policy.md#caller-credential-passthrough). Forward only appropriate end-to-end headers. Replace `accept-encoding` with `identity`: Claude Code asks for Brotli and Anthropic complies, which would hide usage from the [observer](#usage-extraction); an uncompressed response costs only bytes, decompressing a copy costs CPU per event ([research.md](research.md#current-harness-and-baseline)). The caller never selects the upstream destination (no open proxy or SSRF surface). Preserve the configured base-path prefix.

**Backpressure and cancellation.** Downstream body polling applies backpressure. No unbounded queues or detached readers. Client disconnect releases or cancels upstream work, verified by integration tests. Already generated tokens may still be billed.

**Errors.** Before downstream headers are sent, local failures produce a gateway error in the client-facing API's error shape (Anthropic error JSON on `/v1/messages`, OpenAI's on `/v1/responses`). Upstream error bodies pass through unmodified. After headers are sent, an upstream failure terminates the stream; it cannot become a new JSON error response.

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

A provider is a concrete module for one way of reaching a native API: Anthropic and OpenAI (`/v1/responses`) today; Claude Platform on AWS, Bedrock, and Vertex for the same Messages API (M4); OpenAI `/v1/chat/completions` (M3). A closed `Provider` enum (`config::Provider`) selects how Wye authenticates (`x-api-key` or `Authorization: Bearer`), and a closed `Api` enum (`gateway-core/src/api.rs`) holds what differs per native API: the output-token field, the session header, the error shape; there is no trait with one implementation and no translation layer. What differs per provider is only what Wye reads and replaces: endpoint and credential, model id, the usage events (for the usage observer and cost), and the error shape of gateway-generated responses. A request for an endpoint no configured provider serves is rejected, never converted. Passing provider-native options through does not mean Wye interprets or enforces them.

### Upstream connections

hyper-util's client pool keeps a single HTTP/2 connection per host and never opens another, so requests beyond the provider's concurrent-stream limit would queue inside it. `upstream/pool.rs` keeps 64 lanes, each a client with its own pool and therefore its own connection, created at startup and connected lazily. Each request takes a slot on the least loaded lane (ties to the earliest), held by the response body until the stream ends: sequential traffic reuses one connection, `k` concurrent requests use up to `k` connections, and beyond 64 the load stays even. Spreading is deliberate: `h2` guards all streams of one connection with one mutex, and packing streams onto a connection made that lock the dominant cost of the TLS path. Streams on a lane also share its HTTP/2 connection window; with hyper's default (5 MiB, 2 MiB per stream), three clients that stop reading filled it and stalled every other stream on the lane. The connection window is therefore the protocol maximum, and buffering is bounded per stream instead (256 KiB window, measured 0.56 MB of heap): a stalled client holds only its own stream, and only until `client_write_timeout_secs`. The worst case, every in-flight request stalled at once, is `max_concurrent_requests` × 0.56 MB.

### Upstream-held state

Some requests reference state stored by a specific provider account: Responses `previous_response_id` (with `store=true`), stored conversations, uploaded file IDs, and provider cache entries. Wye stores none of it, but routing must respect it:

- A request that references upstream-held state is pinned to the provider and credential that can resolve it, or rejected with an explicit error. It is never routed elsewhere.
- Determining that provider and credential must be stateless: from the route configuration (a route with a single eligible account) or the gateway credential. If neither identifies it, reject.

**As built** (M3 stage 8): a route with one upstream resolves every stored response. With several OpenAI accounts, [`cache_affine`](#cache-affine-routing) already sends every request of a session (or, without a session id, of a credential subject) to the same account, so a `previous_response_id` reaches the account that stored the response as long as the route's upstreams and weights do not change; after a change, a moved session's next reference fails with OpenAI's own `404`, not a misrouted success. Response ids do not name their account, so Wye cannot reject such a request itself. Codex over HTTP sends no `previous_response_id` (it replays the input; [Codex integration](#codex-integration)).

### Cache-affine routing

Provider prompt caches are per account (and sometimes per region), and cached input is priced at a large discount. Cost routing that spreads one conversation across accounts or providers forfeits that discount.

When a route has several upstreams, Wye chooses deterministically by weighted rendezvous hashing over them (`gateway-core/src/route.rs`), keyed by the Claude Code session id when the request carries one, otherwise by the credential subject, otherwise by nothing (every such request picks the same upstream: without `[auth]` Wye serves one loopback user). Each upstream scores `weight / -ln(u)`, with `u` from an xxh3 hash of the key seeded by the upstream's id; the highest score wins, so a key lands on an upstream with probability proportional to its weight. The same session then reaches the same upstream from any replica, with no shared state, and moves only when its upstream leaves the route or the weights change; adding an upstream moves only the keys it now wins. Ids, not positions, seed the hash, so reordering the config moves nothing. A test pins the choice for fixed keys, since a change of hash or scoring would move every session during a rolling update. Strategy name: `cache_affine` (**implemented**, M4 stage 10). Hashing a stable request prefix (system prompt and tools) for SDK callers without a session id is not implemented: it would need the body before choosing. The cost benefit is a hypothesis to measure ([research.md](research.md#questions-to-test)).

Pure affinity can overload one candidate when a few keys dominate traffic, a capacity loss recent serving research reports ([research.md](research.md#2-cache-miss-diagnostics)). The load bound must stay stateless. Implemented: a static share per upstream in config (`weight`), which spreads sessions, not tokens, so one very heavy session still stays on one account. Open: a placement plan fitted offline from telemetry and committed to config. Every replica still computes the same choice.

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

**Selection** (**implemented** for Anthropic and OpenAI accounts, stages 10–11). Candidates are configured per route (`[[routes.claude.upstreams]]`, all serving the same models). Wye picks deterministically, from any replica alike: [`cache_affine`](#cache-affine-routing) by session id (prompt caches are per provider and account, so a session stays on one), weighted by configured shares; or `ordered`, the first candidate in config, the rest only on failover. Not built: excluding a candidate by the request's model or a feature it is known not to support. No health state is shared or remembered between requests. Each upstream keeps its own connection lanes and credential; `forward_caller_credential` stays a single-`[upstream]` mode.

**Failover within a request** (**implemented**, stage 11), only where it cannot duplicate billing: when a candidate answers `429`, `529`, or refuses the connection, before any response byte reached the client, Wye sends the same request to the next candidate in the request's ranking (the next-highest rendezvous score, or config order with `select = "ordered"`), so a failed-over session lands on the same second account from every replica. A `5xx` after the provider may have accepted the request is not retried ([retries](#retries-and-fallback)); config refuses any other reason. Resending needs the request body, which hyper consumes: `wye/src/failover.rs` keeps each chunk it sends (shared `Bytes`, not copies) up to `failover_max_body_bytes` per request (default 8 MiB) and `failover_max_held_bytes` across the process (default 256 MiB, separate from `server.max_held_request_bytes`, so kept bodies never starve the model-permission hold), and when the attempt is dropped hands back the sent chunks plus the unread rest of the client's body through a oneshot channel. A body over either bound, or a refusal that arrives while the upload still runs (the body is not handed back within 100 ms), is answered as it is: never refused, never resent partly. Records carry `failovers` (`[{"upstream":"a","status":429}]`), OTLP `wye.failovers`, and the `wye.failovers` metric counts by refusing upstream and reason. Claude Code still retries on its own, so failover is a latency and availability improvement, not a correctness requirement. A failover costs that turn its prompt-cache discount at the new provider; affinity brings the next turn back.

**Claude Platform on AWS** (**implemented**, API keys only): `provider = "claude-platform-aws"` with `base_url = "https://aws-external-anthropic.{region}.api.aws"` and the required `workspace_id`; Wye sends the key as `Authorization: Bearer` and `anthropic-workspace-id` on every request. Requests, responses, and model ids are the first-party API's, so nothing else changes (platform.claude.com, "Claude Platform on AWS", read 2026-10-26). Its short-term API keys last at most 12 hours and Wye reads keys at startup, so a deployment on them restarts (or rolls) within that window until a key refresher lands; SigV4 (service `aws-external-anthropic`) is the open decision below. Bedrock is not built: its Messages endpoint needs the `anthropic.` model-id prefix, a per-upstream rewrite of `model`, and Anthropic's and AWS's pages disagree on whether its API key goes in `x-api-key` or `Authorization`, to settle with one live request.

**Auth without state.** Each provider credential is resolved at startup like `api_key_env`. SigV4 signs each request on the request path (an HMAC chain, microseconds) with an unsigned payload hash if the service accepts it, since hashing a streamed multi-MB body before sending would mean buffering it; if not, Bedrock and AWS API keys avoid signing. Vertex access tokens expire after about an hour: either Wye exchanges a service-account key for tokens itself (an RS256-signed JWT and one HTTPS call per hour, cached in memory per process), or an external refresher writes a token file Wye re-reads. Both are open decisions below.

**Telemetry and cost.** Usage records carry the upstream's `id` (`upstream`), and OTLP logs, spans, and the request, token, and cost metrics carry `wye.upstream` (**implemented**); `wye report` splits requests, tokens, and cost by upstream. `wye.provider` joins when a second provider exists. Planned: pricing entries scoped to a provider, since partner and regional prices differ ([policy.md](policy.md#pricing-metadata)).

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

Every field except `listen`, `base_url`, and one credential mode is optional; values shown are defaults. `examples/wye.toml` is the annotated version. `[upstream]` and `[routes.claude]` are alternatives: one Anthropic upstream, or several; `[routes.openai]` adds OpenAI, and at least one route is required.

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

# OpenAI Responses for Codex and OpenAI SDKs (/v1/responses); same fields as above.
# [routes.openai]
# [[routes.openai.upstreams]]
# id = "openai"
# provider = "openai"                  # the default here
# base_url = "https://api.openai.com"  # without /v1: clients send /v1/responses
# api_key_env = "OPENAI_API_KEY"       # sent as Authorization: Bearer

# Instead of [upstream]: several Anthropic accounts, chosen per session.
# [routes.claude]
# select = "cache_affine"              # or "ordered": the first in config, the rest only on failover
# failover = ["429", "529", "connect"]  # refusals that prove the request was not processed; nothing else
# failover_max_body_bytes = 8388608     # larger bodies go without failover
# failover_max_held_bytes = 268435456   # kept for resending, across the process
# [[routes.claude.upstreams]]          # up to 16; each takes every [upstream] field above
# id = "team-a"                        # required, unique; in usage records and OTLP
# provider = "anthropic"               # or "claude-platform-aws" (with workspace_id = "wrkspc_..."; the key goes as a bearer token)
# weight = 2                           # share of sessions, 1-1000
# base_url = "https://api.anthropic.com"
# api_key_env = "ANTHROPIC_KEY_TEAM_A" # forward_caller_credential is not allowed here

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

Extends the current schema; `[server]`, `[auth]`, `[telemetry]`, `[cache_diagnostics]`, `[discovery]`, `[[aliases]]`, `[pricing]`, and `[upgrade_check]` keep their fields, and `[upstream]` stays valid as the single-Anthropic-upstream shorthand. `[routes.claude]` with `provider = "anthropic"` upstreams and `cache_affine` exists (above); the rest of this block does not.

```toml
# A route serves one native API, named by the route: claude for /v1/messages
# and /v1/messages/count_tokens, openai for the OpenAI endpoints.
[routes.claude]
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

[routes.openai]                        # /v1/responses, /v1/chat/completions (M3)
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
| Edge support | WASM adapter after native correctness |
| License / package name | After package, domain, and trademark checks |
