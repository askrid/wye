# Architecture

One process per replica, passing each client's native API through to a provider that serves it: the Anthropic Messages API for Claude Code and Anthropic SDK clients, and the OpenAI Responses and Chat Completions APIs for Codex and OpenAI SDK clients. Wye never translates between provider APIs ([product.md](product.md#non-goals)).

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
4. Resolve a model alias; select the route that serves this endpoint natively, and among its upstreams the one policy picks (`cache_affine` or `ordered`).
5. Apply allowlisted rewrites (the resolved model, the provider's model id, agent-aware model routing).
6. Forward upstream unchanged beyond the allowlist.
7. Stream incrementally with downstream backpressure; preserve tool calls and event semantics.
8. Propagate cancellation, apply the retry policy, release request resources.
9. Extract reported usage, or mark it unknown; export telemetry asynchronously.

Implemented for all three APIs: steps 1, 2, 3 (model permissions and output-token ceilings), 4 (aliases, the route per endpoint, the upstream among a route's), 5 for aliases and the provider's model id (Bedrock), 6-8, and 9 as usage records in a JSON-lines log and as OTLP logs, metrics, and spans. Step 8 includes failover before output on routes that configure it. Not built: cost ceilings in step 3, agent-aware routing in step 5.

## Repository and workspace

One repository, four Cargo workspace members:

```text
wye/
├── crates/
│   ├── gateway-core/src/        portable policy, no I/O
│   │   ├── config.rs            wye.toml parsing and validation (config/validate/)
│   │   ├── admission.rs         body admission decisions: hold or not, alias, permission, output-token ceiling, the refusal
│   │   ├── alias.rs             model aliases and per-subject rollout
│   │   ├── api.rs               the native APIs and what differs between them
│   │   ├── credential.rs        gateway credential verification (own and identity-provider) and issuance
│   │   ├── credential/keys.rs   Ed25519, ES256, RS256 keys; JWKS parsing
│   │   ├── credential/provider.rs  identity-provider claims and grants
│   │   ├── policy.rs            model permissions, output-token ceilings, the request-body scanner
│   │   ├── pricing.rs           price table, exact integer cost
│   │   ├── prompt.rs            prompt hashes for cache-miss diagnostics
│   │   ├── route.rs             the upstream for a request and the failover order: weighted rendezvous by session
│   │   ├── upgrade.rs           upgrade-check rules and their request-body scanner
│   │   ├── agent.rs             Claude Code's and Codex's agent headers, bounded and validated
│   │   ├── json.rs              byte-level JSON lexing shared by the scanners
│   │   └── usage.rs             usage and tool-call scanners: Messages streams (usage/raw.rs: the events' shapes), Responses streams (usage/responses.rs, with the
│   │                            response lexer in usage/fields.rs), Chat Completions streams (usage/chat.rs), non-streaming JSON (usage/json.rs)
│   ├── wye/src/                 the binary
│   │   ├── main.rs              command-line entry
│   │   ├── startup.rs           secrets and files the config references, and the upstream credential they make
│   │   ├── server.rs            listener, connections, two-phase shutdown
│   │   ├── server/write_timeout.rs  closes connections whose writes stall
│   │   ├── gateway.rs           the request entry point, routes and their upstreams
│   │   ├── gateway/admit.rs     body admission: holds the body and applies admission.rs's decisions; the model's position
│   │   ├── gateway/forward.rs   a proxied request: permit, admission, sampling, attempts and failover
│   │   ├── gateway/responses.rs gateway-generated responses in the request's API shape
│   │   ├── auth.rs              credential checks, cached per connection
│   │   ├── request_body.rs      the forwarded body: held until `model`, replayed, checked as it streams
│   │   ├── discovery.rs         GET /v1/models
│   │   ├── scanned_body.rs      request bodies read by a scanner as they are sent
│   │   ├── diagnostics.rs       prompt hashes of forwarded request bodies
│   │   ├── upgrade_check.rs     upgrade-check findings of forwarded request bodies
│   │   ├── upstream.rs          forwarding to a provider
│   │   ├── tls.rs               the TLS client every outbound connection uses (upstreams, OTLP, doctor, probe)
│   │   ├── failover.rs          request bodies kept and handed back for failover
│   │   ├── upstream/pool.rs     upstream connection lanes
│   │   ├── upstream/body.rs     response body: idle timeout, connection slot
│   │   ├── upstream/refusal.rs  refusals the request body raised mid-upload, also over HTTP/2
│   │   ├── upstream/connect.rs  the connect timeout over TCP and the TLS handshake
│   │   ├── usage_log.rs         per-request records, bounded writer
│   │   ├── usage_log/           JSON line, OTLP logs, metrics, and traces
│   │   ├── cli/                 args (the command table, parsing, help), service (launchd or systemd unit), paint (terminal color), explain, policy-test, policy-diff, report, doctor, init, claude and codex, probe, token
│   │   ├── gauge.rs             in-flight counters released on drop
│   │   └── alloc_stats.rs       allocation counters (feature alloc-stats, benchmarks only)
│   ├── wye-testkit/             test CA, mock upstream, test signing keys and IdP tokens
│   └── wye-bench/               benchmark, regression gate, load and capacity runs, competitor runs
├── bench/baseline.json
├── scripts/                     check.sh (commit gate), bench-gate.sh, bench-against.sh, ci-bench.sh (CI benchmark), ab.sh, image-smoke.sh, upload-latency.sh
├── .github/                     CI workflows; release.yml is generated by dist (dist-workspace.toml)
├── Dockerfile
└── examples/                    wye.toml, subscription.toml, policy-tests/
```

Create modules only when their behavior is implemented.

- `gateway-core` owns configuration types, validation, credential and request policy, routing, pricing, and the request and response scanners. No Tokio, socket, filesystem, or environment dependencies, so it can later back a WASM adapter.
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
- `POST /v1/responses`: OpenAI Responses, native passthrough to OpenAI (P1: Codex and OpenAI SDK clients), when `[routes.openai]` is set ([Codex integration](#codex-integration)), with usage and cost in records ([usage extraction](#usage-extraction)).
- `POST /v1/chat/completions`: OpenAI Chat Completions, native passthrough to OpenAI (P2: OpenAI SDK clients and OpenAI-compatible agents), on the same `[routes.openai]`.
- `GET /healthz`, `GET /readyz`: operational probes; `HEAD /api/hello`: Claude Code's warm-up probe.
- `GET /v1/models`: with `[discovery]`, the configured catalog narrowed to the models the caller may use (the `[auth] models` ceiling, then the credential's `models` claim), so Claude Code's model picker shows only permitted models. Authenticated when `[auth]` is set; one page (`has_more: false`); no upstream contact and no usage record. Claude Code queries it only with `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`, with a 3-second timeout, and keeps ids containing `claude` or `anthropic`. Codex's catalog request (`GET /v1/models?client_version=...`, its own format) gets `404`, so Codex uses its bundled catalog.

Every other path gets `404 not_found_error`; so does an API whose route is not configured, with a message naming the config section that would serve it.

An `ANTHROPIC_BASE_URL` or `OPENAI_BASE_URL` override works only when the client respects it and uses an implemented endpoint. Base-URL compatibility is not compatibility with proprietary SDK extensions.

### Claude Code gateway contract

Claude Code documents what a gateway must do ([gateway compatibility guide](https://code.claude.com/docs/en/llm-gateway-protocol)). P0 acceptance tests encode it:

- Match on path, not full URL: inference posts to `/v1/messages?beta=true`; forward the query string.
- Forward all `anthropic-*` request headers and all body fields unchanged, as open lists. Never allowlist `anthropic-beta` values: capabilities pair a beta header with a body field, and stripping one half produces `400`s on the next Claude Code release.
- Forward `retry-after`, `x-should-retry`, and `anthropic-ratelimit-unified-*` response headers, and upstream error bodies unmodified; Claude Code's retry and capability-recovery logic matches upstream error wording.
- Relay every SSE event, including `ping`. Claude Code aborts an idle stream and treats a stream that ends before `message_delta`/`message_stop` as dropped.
- Forward the `system` array unchanged; its first block is an attribution block the Anthropic API strips by position.
- Consume, never require, `x-claude-code-session-id`, `x-claude-code-agent-id`, `x-claude-code-parent-agent-id`, and the gateway hint headers (`x-claude-code-request-class`, `-agent-type`, `-compaction`, `-context-compacted`, `-prompt-id`, `-prev-tool-durations`) for telemetry attribution, [agent-native visibility](research.md#agent-native-directions), and, later, [agent-aware routing](research.md#1-agent-aware-routing). A custom base URL receives hint headers only when developers set `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`, as `wye claude` does. Values are client-supplied: bound and validate them before they become telemetry attributes, and never let them authorize anything.
- Answer the best-effort `HEAD /api/hello` warm-up probe with an empty `200` before authentication; it carries no data and reaches no upstream.
- `/v1/messages/count_tokens` is unbilled: it requires authentication and model permission but skips the output-token ceiling (and, once built, cost admission).
- Gateway-generated errors use Anthropic's error JSON shape and status codes, so Claude Code's retry and recovery logic reads them as it reads upstream errors. Wye's own `529 overloaded_error` (concurrency or held-bytes limit, always before any upstream contact) carries `retry-after: 1` and `x-should-retry: true`: integer seconds at most 60, or Claude Code stops retrying.
- Gateway sessions are not eligible for Claude Code's HIPAA configuration; regulated teams must know before adopting Wye.
- Claude Code sends background requests to a second, smaller model unless one is pinned; model permissions must cover it or those requests fail.
- Interactive and background sessions open with a one-token probe (`max_tokens: 1`, no tools, not streamed). With a claude.ai login behind a custom base URL, Anthropic answers it `429 rate_limit_error`, with or without Wye in the path (checked 2026-10-30), and Claude Code carries on, so one 429 per session start in the usage log is expected.

The guide changes with the client. Re-run the contract tests against each targeted Claude Code release.

### Codex integration

What Codex sends a custom provider, from its source (openai/codex at `970111f`, read 2026-10-26) and its gateway guide (learn.chatgpt.com, "Gateway compatibility"):

- A custom `[model_providers.<id>]` with `wire_api = "responses"` gets `POST {base_url}/responses` over HTTP with SSE; its `supports_websockets` defaults to false, so no WebSocket transport. The built-in `openai` provider (also reached through `openai_base_url`) tries WebSockets first and falls back to HTTP; `wye codex` therefore defines its own provider.
- The bearer token comes from the environment variable the provider's `env_key` names, as `Authorization: Bearer`: with `[auth]` a gateway credential, which Wye verifies and replaces with the OpenAI key. Request bodies are zstd-compressed only with a ChatGPT login on the built-in provider, never for an API-key custom provider, so Wye reads plain JSON. Codex serializes `model` first ("gateways may inspect request bodies incrementally"), so the model hold usually reads one chunk.
- Headers for attribution, mapped onto the fields Claude Code's headers fill (implemented): `session-id` → `session_id` (and affinity, and the session trace); `thread-id` → `agent_id` and `x-codex-parent-thread-id` → `parent_agent_id` (a thread is one agent's conversation); `x-openai-subagent` → `agent_type`, and `request_class` (`collab_spawn` → `subagent`, `compact` → `compaction`, `review` and `memory_consolidation` → `auxiliary`, unknown → `other`). Codex omits the header only on the main thread, so a request without it is `main` when its `originator` names Codex (`codex_cli_rs`, ...), and unclassified from other clients. `x-codex-turn-metadata` (JSON, with the request kind) is not read: the request path reads only headers. Over HTTP Codex replays the whole input each turn and sends no `previous_response_id` (that is WebSocket-only), so no upstream-held state pins a request yet.
- Retries: `request_max_retries` (default 4) on 5xx and transport errors, never on `429`; `stream_max_retries` (5) for interrupted streams. Wye's overload answer on Responses is therefore `503` with `retry-after`, which Codex and the OpenAI SDKs retry.

Gateway-generated errors on `/v1/responses` use OpenAI's shape: `{"error": {"message", "type", "param", "code"}}`, with `type` `invalid_request_error` or `server_error` and `code` naming Wye's reason (`permission_error`, `authentication_error`, `overloaded_error`, ...).

## Forwarding contract

Each endpoint forwards to the corresponding upstream endpoint without converting between APIs.

1. Validate the endpoint and bounded request size.
2. Construct the configured upstream URL and headers.
3. Send through the shared client.
4. Forward upstream status and permitted headers.
5. Stream the response body as the downstream consumes it.

**Bodies.** Preserve request JSON, unknown fields, tool definitions, and provider-native options. The request body streams to the upstream unchanged beyond the allowlist below, bounded by `server.max_request_body_bytes` (default 32 MiB, Anthropic's limit), counted on the bytes the client sent: a declared length past it gets `413` at once, and a body without one gets `413` when it passes the limit mid-upload. With [model permissions or output-token ceilings](policy.md#gateway-credentials), aliases, or a provider with its own model ids, only the start of the body is held, until its top-level `model` has been read; the held bytes are bounded across the process by `server.max_held_request_bytes`, and the body is never re-serialized. Never buffer the generated response. Relay SSE bytes without parsing or rebuilding events; HTTP chunks are not SSE event boundaries. Preserve non-streaming JSON and upstream error bodies.

**Request rewrites.** Wye rewrites only an explicit allowlist of request fields; everything else passes through byte-equivalent in meaning:

| Field | When | Why |
|---|---|---|
| `model` | [Model alias](#model-aliases-and-upgrade-checks) (implemented, M2b); a Claude provider's model id (Bedrock, implemented); agent-aware routing (planned, M4) | Replace an alias with the model config maps it to, a model policy selects for the request class, or the selected provider's id for the same model (`anthropic.claude-opus-5-5` on Bedrock). Never a different model family |
| `model` → URL, `anthropic_version` | Vertex AI (planned, M4) | Vertex names the model in the URL and needs `"anthropic_version": "vertex-2023-10-16"` in the body: the top-level `model` member is cut and the version member spliced in; every other byte is unchanged |
| `max_tokens` / `max_completion_tokens` / `max_output_tokens` | Hard cost mode (planned) | Insert the output bound used in the [cost ceiling](policy.md#provable-cost-ceiling) when absent; lower a caller value only when the route explicitly accepts truncation, otherwise reject; never raise a caller value. On Messages, never lower below `thinking.budget_tokens`; reject instead |
| `stream_options.include_usage` | Streaming Chat Completions with usage accounting (planned, off: see open decisions) | OpenAI omits streamed usage otherwise ([usage extraction](#usage-extraction)); until the default is decided, clients that want usage set it themselves |

Each rewrite is per-route configuration, covered by tests, and reported by `wye explain`. Adding a field to this list is a design change.

### Usage extraction

A bounded, read-only observer reads the same bytes Wye relays and recognizes only usage-bearing events; it never delays, buffers, or changes them, and an oversized or malformed event marks usage `unknown` instead of failing the stream.

For Anthropic Messages streams (`gateway-core/src/usage.rs`): at most one partial line is kept (32 bytes for ordinary events, 4 KiB for `content_block_start`, 64 KiB for usage events), line ends are found with SIMD `memchr`, and JSON is parsed only for `message_start` and `message_delta` (usage) and `content_block_start` (tool names, borrowed). A body wrapper emits one record when the body is dropped, so completed, failed, cancelled, and gateway-error outcomes are all recorded; a request the client leaves before its response headers (while Wye holds its body or waits on the upstream) is recorded as cancelled with nginx's status `499`. A body also counts as completed once its declared length is delivered: the client connection stops polling then, and an HTTP/2 upstream may send its end-of-stream frame after the data. Messages streams always carry usage, so no rewrite is needed.

For Chat Completions streams (`gateway-core/src/usage/chat.rs`): only `data:` lines, found with `memchr`, and parsed only when a substring search (`memmem`) shows they matter: the first chunk (the model), a chunk with `"usage":{`, or one starting a tool call (`"tool_calls"` with a `"name":"` key, which escaped quotes inside argument strings never match). Usage arrives only in a final chunk the client asks for with `stream_options.include_usage`; Wye does not add the field (the [rewrite](#forwarding-contract) is an open decision: the extra chunk with empty `choices` trips some clients), so a stream without it records `unknown`. `prompt_tokens` include cached tokens and are split like Responses' `input_tokens`.

Non-streaming JSON responses (`gateway-core/src/usage/json.rs`, all three APIs; Chat's tool calls sit in `choices` and are not counted) carry `model` and `usage` as top-level members, after the generated content. The same constant-memory lexer (`usage/fields.rs`) reads the body as it is relayed: it keeps `model`, `service_tier`, and the `usage` object (at most 2 KiB) and counts tool calls among the top-level `content` blocks (Messages `tool_use`) or `output` items (Responses calls), reading only each item's `type` and `name`; text and tool inputs are skipped. A body cut off before `usage` is `unknown`. Error responses, `count_tokens` (which generates nothing), and responses that are neither SSE nor JSON are `not_observed`.

For OpenAI Responses streams (`gateway-core/src/usage/responses.rs`): usage arrives only in the terminal event (`response.completed`, `response.incomplete`, `response.failed`), whose data line carries the whole response, output included, often megabytes. That line is lexed as it streams with constant memory, keeping only `response.model`, `response.service_tier`, and the `response.usage` object (at most 2 KiB); strings inside the output are skipped with `memchr`, so a lookalike `"usage"` in generated text is never read. `response.output_item.added` lines (at most 4 KiB, an item without its content) give tool calls by name, and hosted tools billed per call mark the cost partial. Other events are skipped after their `event:` line. A stream cut before the terminal event has no usage (`unknown`); there is no `partial` here, because OpenAI sends no interim counts. `UsageScanner` picks the scanner per API as an enum, so the per-chunk call is a match.

**Headers and destination.** Strip hop-by-hop headers, including those named by `Connection`. Replace caller authorization (`Authorization` or `x-api-key`) with the configured provider credential, except in explicit loopback [caller-credential passthrough](policy.md#caller-credential-passthrough); with the provider credential, also drop the caller's `OpenAI-Organization` and `OpenAI-Project`, which would choose what the operator's key bills. Forward only appropriate end-to-end headers. Replace `accept-encoding` with `identity`: Claude Code asks for Brotli and Anthropic complies, which would hide usage from the [observer](#usage-extraction); an uncompressed response costs only bytes, decompressing a copy costs CPU per event ([research.md](research.md#current-harness-and-baseline)). The caller never selects the upstream destination (no open proxy or SSRF surface). Preserve the configured base-path prefix.

**Backpressure and cancellation.** Downstream body polling applies backpressure. No unbounded queues or detached readers. Client disconnect releases or cancels upstream work, verified by integration tests. Already generated tokens may still be billed.

**Errors.** Before downstream headers are sent, local failures produce a gateway error in the client-facing API's error shape (Anthropic error JSON on the Messages endpoints, OpenAI's on `/v1/responses` and `/v1/chat/completions`). Upstream error bodies pass through unmodified. A refusal Wye's request body raises after forwarding began (a late output-token field, a repeated `model`, the size limit) aborts the upstream request and gets its own status, from an HTTP/1.1 or HTTP/2 upstream alike: hyper's HTTP/1.1 client returns the body's error as the cause, while its HTTP/2 client only resets the stream, so on an upstream that may speak HTTP/2 the body also leaves the refusal in a per-request slot. After headers are sent, an upstream failure terminates the stream; it cannot become a new JSON error response.

**Timeouts and shutdown.** Use three timeouts: a short connect timeout, over the TCP connect and the TLS handshake together (a peer that accepts and then stalls the handshake fails as a refused connection, so failover applies); a response-header timeout of several minutes, because a non-streaming generation sends no headers until it completes (Claude Code falls back to non-streaming requests after a stream failure); and a streaming-inactivity timeout longer than the provider's `ping` interval. A fourth bounds the client side: a connection whose response writes make no progress for `client_write_timeout_secs` is closed, cancelling its upstream request, because a client that stops reading leaves its response unpolled and the inactivity timer never runs. No total request timeout: it would kill valid generations. Shutdown has a drain phase (readiness fails, requests still served) and a grace phase that cancels whatever remains ([operations.md](operations.md#health-and-shutdown)).

## Retries and fallback

Wye does not retry an ambiguous failure or fall back after output: an ambiguous failure may already have incurred charges, and switching providers after output starts cannot preserve the response. On `/v1/messages`, Wye does not retry the same provider: Claude Code retries on its own, guided by the `x-should-retry` and `retry-after` headers Wye forwards, and gateway retries would multiply attempts and risk duplicate billing. The one exception, implemented, is [failover to another account or Claude provider](#claude-across-providers-m4) after a refusal that proves the request was not processed (`429`, `529`, a connection refused or cut before the body was delivered whole), before any response byte was sent. When retries are added for other routes, they follow this policy:

| Failure stage | Policy |
|---|---|
| Local validation, before upstream send | No upstream work; return error or choose another eligible route |
| Connect failure before the request reaches the provider | Retry may be safe within a bounded deadline |
| Upstream may have accepted the request, no response yet | Retry risks duplicate billing; conservative |
| Output already sent downstream | Never transparently replay from another model |
| Partial tool-call payload emitted | No automatic fallback without higher-level recovery semantics |

Keep request IDs and trace linkage. Do not claim exactly-once execution.

## Providers

A provider is a concrete module for one way of reaching a native API: Anthropic, Claude Platform on AWS, and Bedrock for the Messages API, and OpenAI for `/v1/responses` and `/v1/chat/completions`, today; Vertex for the Messages API is planned (M4). A closed `Provider` enum (`config::Provider`) selects how Wye authenticates (`x-api-key` or `Authorization: Bearer`), any header the provider requires (Claude Platform on AWS's workspace), and its model-id prefix (Bedrock's `anthropic.`), and a closed `Api` enum (`gateway-core/src/api.rs`) holds what differs per native API: the output-token field, the session header, the error shape; there is no trait with one implementation and no translation layer. What differs per provider is only what Wye reads and replaces: endpoint and credential, model id, the usage events (for the usage observer and cost), and the error shape of gateway-generated responses. A request for an endpoint no configured provider serves is rejected, never converted. Passing provider-native options through does not mean Wye interprets or enforces them.

### Upstream connections

hyper-util's client pool keeps a single HTTP/2 connection per host and never opens another, so requests beyond the provider's concurrent-stream limit would queue inside it. `upstream/pool.rs` keeps 64 lanes, each a client with its own pool and therefore its own connection, created at startup and connected lazily. Each request takes a slot on the least loaded lane (ties to the earliest), held by the response body until the stream ends: sequential traffic reuses one connection, `k` concurrent requests use up to `k` connections, and beyond 64 the load stays even. Spreading is deliberate: `h2` guards all streams of one connection with one mutex, and packing streams onto a connection made that lock the dominant cost of the TLS path. Streams on a lane also share its HTTP/2 connection window; with hyper's default (5 MiB, 2 MiB per stream), three clients that stop reading filled it and stalled every other stream on the lane. The connection window is therefore the protocol maximum, and buffering is bounded per stream instead (256 KiB window, measured 0.56 MB of heap): a stalled client holds only its own stream, and only until `client_write_timeout_secs`. On the client side, the kernel send buffer of each connection is fixed at 128 KiB (`server.rs`; Linux doubles the value, so 256 KiB there) instead of autotuning to as much as 4 MiB of kernel memory. The worst case, every in-flight request stalled at once, is `max_concurrent_requests` × about 0.7 MB.

### Upstream-held state

Some requests reference state stored by a specific provider account: Responses `previous_response_id` (with `store=true`), stored conversations, uploaded file IDs, and provider cache entries. Wye stores none of it, but routing must respect it:

- A request that references upstream-held state is pinned to the provider and credential that can resolve it, or rejected with an explicit error. It is never routed elsewhere.
- Determining that provider and credential must be stateless: from the route configuration (a route with a single eligible account) or the gateway credential. If neither identifies it, reject.

As built in M3 stage 8, a route with one upstream resolves every stored response. With several OpenAI accounts, [`cache_affine`](#cache-affine-routing) already sends every request of a session (or, without a session id, of a credential subject) to the same account, so a `previous_response_id` reaches the account that stored the response as long as the route's upstreams and weights do not change; after a change, a moved session's next reference fails with OpenAI's own `404`, not a misrouted success. Response ids do not name their account, so Wye cannot reject such a request itself. Codex over HTTP sends no `previous_response_id` (it replays the input; [Codex integration](#codex-integration)).

### Cache-affine routing

Provider prompt caches are per account (and sometimes per region), and cached input is priced at a large discount. Cost routing that spreads one conversation across accounts or providers forfeits that discount.

When a route has several upstreams, Wye chooses deterministically by weighted rendezvous hashing over them (`gateway-core/src/route.rs`), keyed by the client's session id (Claude Code's or Codex's) when the request carries one, otherwise by the credential subject, otherwise by nothing (every such request picks the same upstream: without `[auth]` Wye serves one loopback user). Each upstream scores `weight / -ln(u)`, with `u` from an xxh3 hash of the key seeded by the upstream's id; the highest score wins, so a key lands on an upstream with probability proportional to its weight. The same session then reaches the same upstream from any replica, with no shared state, and moves only when its upstream leaves the route or the weights change; adding an upstream moves only the keys it now wins. Ids, not positions, seed the hash, so reordering the config moves nothing. A test pins the choice for fixed keys, since a change of hash or scoring would move every session during a rolling update. Strategy name: `cache_affine` (implemented, M4 stage 10). Hashing a stable request prefix (system prompt and tools) for SDK callers without a session id is not implemented: it would need the body before choosing. The cost benefit is a hypothesis to measure ([research.md](research.md#questions-to-test)).

Pure affinity can overload one candidate when a few keys dominate traffic, a capacity loss recent serving research reports ([research.md](research.md#2-cache-miss-diagnostics)). The load bound must stay stateless. Implemented: a static share per upstream in config (`weight`), which spreads sessions, not tokens, so one very heavy session still stays on one account. Open: a placement plan fitted offline from telemetry and committed to config. Every replica still computes the same choice.

### Model aliases and upgrade checks

#### Aliases

Implemented in M2b stage A. A client asks for a stable name and config decides the model:

```toml
[[aliases]]
name = "coding-default"
model = "claude-opus-5-5"
# Staged rollout: credentials whose subject matches get another model first.
when = [{ subjects = ["team-canary-*"], model = "claude-sonnet-5-5" }]
```

- Resolution reads the top-level `model` with the existing [policy scanner](policy.md#gateway-credentials) hold (Claude Code and the SDKs send `model` first; configuring `[[aliases]]` makes the hold apply to every request) and splices the resolved name in place of the alias: the held chunks are sliced around the value, so only the new name is allocated, every other byte is unchanged, and `content-length` is adjusted by the difference. Names that are not aliases pass through untouched, so aliases are opt-in per request.
- Rollout: `when` rules match the credential's subject (exact or a prefix ending in `*`, as in model permissions); the first match wins, and callers without a credential get the alias's `model`. Resolution is one step: an alias names a model, never another alias.
- Policy applies to the model that runs: model permissions apply to the resolved model, and pricing to the model the provider reports, which is the resolved one. `GET /v1/models` lists aliases after the catalog's models, each when the model it resolves to for that caller is permitted. Claude Code's model picker keeps only ids containing `claude` or `anthropic`, so aliases meant for it are named accordingly (`claude-coding`).
- Visibility: usage records carry `requested_model` (the alias) next to `model` (as the provider reports it), OTLP logs `wye.requested_model`; `wye explain` prints the resolution, and a policy test can pin the resolved model (`model = "claude-opus-5-5"` in its `.expect.toml`), so a rollout is reviewed like any policy change.
- Cost, measured with every benchmark request aliased: one allocation per request for the hold (as with model permissions), one more when the name's length changes (the new `content-length`), CPU per request within noise. With aliases alone the rest of the body is not lexed: the repeated-`model` check protects permissions, which do not apply (Claude Code-shaped uploads 1.64 vs 1.49 ms per MB, within noise; with the check, 2.16).
- An alias names one model. Mapping one name to several providers of that model is [M4](#claude-across-providers-m4)'s route, not a second alias mechanism.

#### Upgrade checks

Implemented in M2b stage B. Moving an alias, or a client, to a new model fails in production when the new model rejects what the old one accepted. Wye ships a rule table of each model's breaking request changes, every rule citing the provider's migration notes and covered by its own test (`gateway-core/src/upgrade.rs`):

| Rule | Request field | Rejected by (dated ids included) |
|---|---|---|
| `thinking_disabled` | `thinking.type` `disabled` | Opus 5.5; Sonnet 5.5; Fable 5, 5.1; Mythos 5.1 (Opus 5 and Haiku 5.5 only at effort `xhigh` or `max`, not reported) |
| `budget_tokens` | `thinking.type` `enabled` (manual budget) | Opus 4.7, 4.8, 5, 5.5; Sonnet 5, 5.5; Haiku 5.5; Fable 5, 5.1; Mythos 5.1 |
| `forced_tool_choice` | `tool_choice.type` `any` or `tool` | Opus 5.5; Sonnet 5.5; Fable 5.1; Mythos 5.1 |
| `computer_tool` | `tools[].type` `computer_20250124` or `computer_20251124` | Opus 5.5, Sonnet 5.5, Haiku 5.5 on the Claude API and Google Cloud (Bedrock still takes `computer_20251124` on Opus and Sonnet 5.5) |
| `prefill` | the last `messages[]` entry is an assistant turn | Opus 4.6 and later; Sonnet 4.6, 5, 5.5; Haiku 5.5; Fable 5, 5.1; Mythos 5.1 |
| `sampling` | `temperature`, `top_p`, or `top_k` | Opus 4.7 and later (any value); Sonnet 5, 5.5, Haiku 5.5, Fable 5, 5.1, Mythos 5.1 (non-default values; reported for any, since the guide says to omit them) |
| `system_role` | a `messages[]` entry with role `system` | Opus 4.6, 4.7; Sonnet 4.6, 5 (mid-conversation system messages start with Opus 4.8) |

Source: Anthropic's model migration guide, its per-model sections (the [Opus 5.5 guide](https://platform.claude.com/docs/en/models/opus-5-5/migration-guide) read 2026-10-26, the models per rule re-read 2026-10-30). Only rejections the body shows are rules: the effort level, the platform, and a parameter left at its default can make a listed model accept the request. The deprecated `output_format` is rejected without a beta header on every model, so it is not an upgrade rule.

```toml
[upgrade_check]
targets = ["claude-opus-5-5"]          # models an alias or a client may move to; up to 16
sample_percent = 10                    # share of sessions (or, without one, clients) checked; default 100
```

- Scan: for sampled requests, a constant-memory structure scanner on the forwarded body reads only the fields the rules name (keys at the first three levels; `thinking.type`, `tool_choice.type`, `tools[].type`, `messages[].role`) and records which rules each target breaks. Sampling is by Claude Code session id, or by the credential's subject for SDK callers without one, alike on every replica. Cost: on a Claude Code-shaped 1 MiB body it scans in 0.77× the time of the model-permission scan and half that of cache diagnostics (same machine, same body), so about 0.5 ms per MB in the gateway.
- Records: `upgrade_issues` in the usage record (`{"claude-opus-5-5": ["budget_tokens", "prefill"]}`; `{}` for a checked request no target rejects), `wye.upgrade_issues` in OTLP logs, and the `wye.upgrade.issues` metric by target, rule, and subject. The backend shows which clients would fail before the switch.
- Offline: `wye explain --config wye.toml --target-model claude-opus-5-5 request.json` (repeatable; without it, the configured targets) prints each issue with the fix and its source; `wye report` lists issues by target, rule, and client.
- Report only: the request is forwarded unchanged ([non-goal](product.md#non-goals)). The rule table is versioned with Wye.

### Claude across providers (M4)

Status: several accounts, `cache_affine` and `ordered` selection, failover before output, and Claude Platform on AWS and Bedrock with API keys are implemented; Vertex, SigV4, and excluding a candidate by model or feature are planned.

The same Claude models are sold by four providers. Wye can serve one Claude Code fleet from several of them, choosing per request and failing over, because all four speak the Anthropic Messages API with small, known differences. Wye only chooses among them: no request is translated or sent to another model family.

| Provider | Endpoint | Request | Stream | Auth | Model id |
|---|---|---|---|---|---|
| Anthropic | `api.anthropic.com/v1/messages` | Messages | SSE | `x-api-key` | `claude-opus-5-5` |
| Claude Platform on AWS (Anthropic-operated) | `aws-external-anthropic.{region}.api.aws/v1/messages` | Messages, same-day parity | SSE | SigV4 (`aws-external-anthropic`) or short-term API key; workspace id required | `claude-opus-5-5` |
| Amazon Bedrock, Messages endpoint (Opus 4.7 and later) | `bedrock-mantle.{region}.api.aws/anthropic/v1/messages` | Messages | SSE | SigV4 (`bedrock-mantle`) or a Bedrock API key in `x-api-key` | `anthropic.claude-opus-5-5` |
| Google Vertex AI | `aiplatform.googleapis.com/v1/projects/{p}/locations/{loc}/publishers/anthropic/models/{model}:streamRawPredict` | Messages without `model`, with `anthropic_version` | SSE (to verify) | OAuth access token (`Authorization: Bearer`) | `claude-opus-5-5` |

Sources: Anthropic's platform pages for Bedrock, Vertex, and Claude Platform on AWS (read 2026-10-22). The legacy Bedrock `InvokeModel` path streams AWS event-stream frames, not SSE, so it would need re-framing per event and is excluded. Partner platforms lack some first-party features (Files API, URL sources, some server tools, `fallbacks`); a request that uses one fails at that provider with the provider's own error, and Wye does not hide the difference.

**Selection** (implemented, stages 10-11; Anthropic, Claude Platform on AWS, Bedrock, and OpenAI accounts). Candidates are configured per route (`[[routes.claude.upstreams]]`, all serving the same models). Wye picks deterministically, from any replica alike: [`cache_affine`](#cache-affine-routing) by session id (prompt caches are per provider and account, so a session stays on one), weighted by configured shares; or `ordered`, the first candidate in config, the rest only on failover. Not built: excluding a candidate by the request's model or a feature it is known not to support. No health state is shared or remembered between requests. Each upstream keeps its own connection lanes and credential; `forward_caller_credential` stays a single-`[upstream]` mode.

**Failover within a request** (implemented, stage 11) happens only where it cannot duplicate billing, and only before any response byte reached the client. A `429`, a `529`, a refused connection, or a connection lost before the request body was delivered whole moves the request to the next candidate in its ranking: the next-highest rendezvous score, or config order with `select = "ordered"`, so every replica fails a session over to the same second account. An incomplete request cannot have been processed, which covers a provider that refuses early and closes during the upload. A `5xx` after the provider may have accepted the request is not retried ([retries](#retries-and-fallback)), and config refuses any other reason.

Resending needs the body, which hyper consumes. `wye/src/failover.rs` keeps each chunk it sends (shared `Bytes`, not copies), up to `failover_max_body_bytes` per request (default 8 MiB) and `failover_max_held_bytes` across the process (default 256 MiB, separate from `server.max_held_request_bytes`, so kept bodies never starve the hold that reads `model`). A dropped attempt hands back its sent chunks plus the unread rest of the client's body through a oneshot channel, so a refusal that arrives mid-upload moves on too: the next upstream gets the sent bytes, then the rest as the client sends it. A body over either bound, or an attempt still holding its body 100 ms after the refusal (hyper still writing), is answered as it is, never refused and never partly resent.

Records carry `failovers` (`[{"upstream":"a","status":429}]`), OTLP logs `wye.failovers`, and the `wye.failovers` metric counts refusals by upstream and reason. Claude Code retries on its own, so failover improves latency and availability but is not needed for correctness. A failover costs that turn its prompt-cache discount at the new provider; affinity brings the next turn back.

**Claude Platform on AWS** (implemented, API keys only): `provider = "claude-platform-aws"` with `base_url = "https://aws-external-anthropic.{region}.api.aws"` and the required `workspace_id`; Wye sends the key as `Authorization: Bearer` and `anthropic-workspace-id` on every request. Requests, responses, and model ids are the first-party API's, so nothing else changes (platform.claude.com, "Claude Platform on AWS", read 2026-10-26). Its short-term API keys last at most 12 hours and Wye reads keys at startup, so a deployment on them restarts (or rolls) within that window until a key refresher lands; SigV4 (service `aws-external-anthropic`) is the open decision below.

**Amazon Bedrock** (implemented with API keys, not yet checked against the live service): `provider = "bedrock"` with `base_url = "https://bedrock-mantle.{region}.api.aws/anthropic"`. Wye sends the Bedrock API key as `x-api-key`, as Anthropic's page says for the standard SDK; AWS's page shows `Authorization: Bearer`, and [one live request](research.md#verification-needed) will settle it. Bedrock model ids carry an `anthropic.` prefix. On a route with a Bedrock upstream, Wye holds each request until its `model` (as for model permissions) and gives each attempt its upstream's id: `claude-opus-5-5` becomes `anthropic.claude-opus-5-5` for Bedrock and stays as sent for the others, with `content-length` adjusted; an id that already has the prefix is kept. Records, OTLP, and pricing see the model the provider reports in its response; whether Bedrock reports its own `anthropic.` id there, which a price entry would then need to name, is part of the live check. A failover resend gets the next upstream's id wherever the value is, in the bytes already sent or in the unread rest; a value split between the two (a refusal in the middle of the first chunk) is answered as it is. Routes without a Bedrock upstream are unchanged.

**Auth without state** (planned for SigV4 and Vertex). Each provider credential is resolved at startup like `api_key_env`. SigV4 would sign each request on the request path (an HMAC chain, microseconds) with an unsigned payload hash if the service accepts it, since hashing a streamed multi-MB body before sending would mean buffering it; if not, Bedrock and AWS API keys avoid signing. Vertex access tokens expire after about an hour: either Wye exchanges a service-account key for tokens itself (an RS256-signed JWT and one HTTPS call per hour, cached in memory per process), or an external refresher writes a token file Wye re-reads. Both are open decisions below.

**Telemetry and cost** (implemented). Usage records carry the upstream's `id` (`upstream`), OTLP logs, spans, and the request, token, and cost metrics carry `wye.upstream`, and `wye report` splits requests, tokens, and cost by upstream. There is no separate `wye.provider`: the `id` names the account, and config maps it to its provider. Partner and regional prices differ, so pricing entries can be scoped to upstreams (`upstreams = ["aws-east"]`; [policy.md](policy.md#pricing-metadata)).

### Tools and MCP

```text
Agent ──tools/schema──> Wye ──> model
Agent <─tool_calls───── Wye <── model
Agent ──executes tool / uses MCP server independently
Agent ──tool results─> Wye ──> model
```

This pattern needs no MCP gateway. Provider-hosted tools pass through like any other field; Wye neither runs nor rewrites them.

## Configuration

One TOML file, loaded at startup, is the source of truth for routes and their providers, credential verification, limits, aliases, and prices. Secrets are referenced by environment variable, never stored in Git, images, or IaC state.

The runtime resolves environment variables; the core validates configuration without resolving secrets. Reject unknown fields, invalid URLs or limits, broken references, duplicate IDs, unsupported provider/API pairs, missing credentials, and a policy that needs prices without them at startup. Never print secrets or log request bodies by default.

No hot reload; changes require a restart (or rolling deploy). A config snapshot stays consistent within a request. Planned: include the config version in logs and traces to diagnose rollout skew.

### Current schema (implemented)

[`examples/wye.toml`](../examples/wye.toml) is the reference: every field with its default, its limits, and what it does, kept valid by a test that parses it with every section enabled. Only `listen`, `base_url`, and one credential are required; unknown fields are rejected at startup. `[upstream]` and `[routes.claude]` are alternatives (one Anthropic upstream, or several), `[routes.openai]` adds OpenAI, and at least one route is required.

| Section | Configures | Design |
|---|---|---|
| `[server]` | the listener; body, held-byte, and concurrency limits; drain and shutdown; the client write timeout | [operations.md](operations.md#health-and-shutdown) |
| `[upstream]`, `[routes.claude]` | Anthropic Messages upstreams: base URL, provider, credential, timeouts, HTTP version, CA file; for a route, `select`, `weight`, `failover`, and its memory bounds | [Claude across providers](#claude-across-providers-m4), [cache-affine routing](#cache-affine-routing) |
| `[routes.openai]` | OpenAI Responses and Chat Completions upstreams | [Codex integration](#codex-integration) |
| `[auth]`, `[[auth.keys]]`, `[[auth.issuers]]` | gateway credentials, model permissions, the output-token ceiling | [policy.md](policy.md#gateway-credentials) |
| `[cache_diagnostics]` | keyed prompt hashes in usage records | [research.md](research.md#2-cache-miss-diagnostics) |
| `[telemetry]` | OTLP logs, metrics, and traces | [operations.md](operations.md#telemetry) |
| `[discovery]` | the `GET /v1/models` catalog | [Public API](#public-api) |
| `[[aliases]]`, `[upgrade_check]` | model aliases and upgrade checks | [aliases and upgrade checks](#model-aliases-and-upgrade-checks) |
| `[pricing]` | the price table for costs | [policy.md](policy.md#pricing-metadata) |

Config defaults are the `DEFAULT_*` constants in `gateway-core/src/config.rs`; the limits a config may set (16 upstreams per route, 256 aliases, ...) are `MAX_*` constants beside their types, and `examples/wye.toml` states those an operator sets (validation errors name the rest, such as the format of an upstream id).

### Fixed limits

Bounds operators should not need to tune are compile-time constants, each documented where it is used with its value and the measurement behind it. Changing one goes through the benchmark gate like any code change.

| Constant | File | Bounds |
|---|---|---|
| `LANES` | `wye/src/upstream/pool.rs` | HTTP/2 connections per upstream host ([upstream connections](#upstream-connections)) |
| `STREAM_WINDOW`, `CONNECTION_WINDOW` | `wye/src/upstream.rs` | Response bytes Wye buffers per upstream stream; the connection window is the protocol maximum |
| `MAX_CONNECTION_BUFFER` | `wye/src/server.rs` | A client connection's read buffer, which also bounds request headers |
| `CLIENT_SEND_BUFFER` | `wye/src/server.rs` | A client connection's kernel send buffer, so a stalled client holds little kernel memory |
| `HANDBACK_WAIT` | `wye/src/gateway/forward.rs` | How long a refused attempt may take to hand its body back for failover |
| `MIN_CHUNK_CHARGE` | `wye/src/request_body.rs` | The least a held or kept chunk counts against `max_held_request_bytes` and `failover_max_held_bytes` (1 KiB), so a body sent in tiny chunks cannot pin more memory than the budget says |
| `HOLD_IDLE_TIMEOUT` | `wye/src/request_body.rs` | How long a held body may send nothing before its request gets `408` (60 s) |
| `QUEUE_CAPACITY` | `wye/src/usage_log.rs` | Usage records waiting for the writer task; beyond it they are dropped and counted |
| `WRITE_BATCH` | `wye/src/usage_log.rs` | Queued records formatted into one write: bounds the write buffer while records keep arriving |
| `MAX_BATCH_RECORDS`, `EXPORT_QUEUE`, `EXPORT_TIMEOUT` | `wye/src/usage_log/otlp.rs` | OTLP batch size, batches waiting for export, one export attempt |
| `OTLP_FLUSH_INTERVAL`, `OTLP_METRICS_INTERVAL` | `wye/src/usage_log.rs` | How often a partial batch and the cumulative metrics are exported |
| `MAX_SERIES` | `wye/src/usage_log/metrics.rs` | Attribute sets per OTLP metric |
| `LOG_FLUSH_TIMEOUT` | `wye/src/server.rs` | How long shutdown waits for the usage log |
| `STOP_WAIT` | `wye/src/main.rs` | How long the process waits on blocking work, such as a write to a stderr nobody reads, before it exits anyway |
| `ACCEPT_ERROR_PAUSE` | `wye/src/server.rs` | The pause after an accept error such as file descriptor exhaustion, which would otherwise spin a core |
| `MAX_TOKEN_BYTES`, `MAX_JWKS_BYTES` | `gateway-core/src/credential.rs`, `credential/keys.rs` | A presented credential and an identity provider's key set |
| `CLOCK_SKEW_SECS` | `gateway-core/src/credential.rs` | How far a credential's `iat` or `nbf` may lie ahead of this clock |
| `MAX_TIMER_SECS` | `gateway-core/src/config/validate.rs` | Any configured timeout or interval (one day): larger values would overflow a deadline |
| `MAX_USAGE_LINE_BYTES`, `MAX_BLOCK_START_LINE_BYTES`, `MAX_TOOL_NAMES` | `gateway-core/src/usage.rs` | A usage-bearing Messages SSE line; a `content_block_start` line; tool names counted per response |
| `MAX_ITEM_LINE_BYTES` | `gateway-core/src/usage/responses.rs` | A Responses `response.output_item.added` line |
| `MAX_LINE_BYTES` | `gateway-core/src/usage/chat.rs` | A Chat Completions `data:` line kept across chunks |
| `MAX_USAGE_BYTES`, `MAX_FIELD_BYTES` | `gateway-core/src/usage/fields.rs` | The `usage` object of a JSON response or a Responses terminal event; its `model` and `service_tier` |
| `MAX_MODEL_BYTES` | `gateway-core/src/policy.rs` | A `model` value the policy scanner reads |
| `MAX_TOKEN_BYTES`, `MAX_TOOL_ENTRIES` | `gateway-core/src/agent.rs` | An agent id or type from a client header; tool durations read per request |

### Target schema (illustrative, not implemented)

What the planned work adds to the [current schema](#current-schema-implemented); nothing in this block exists yet.

```toml
# AWS SigV4 instead of an API key, for Bedrock and Claude Platform on AWS (open decision).
[[routes.claude.upstreams]]
id = "bedrock-us"
provider = "bedrock"
base_url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"
aws_credentials = "environment"

# Vertex AI: the model goes in the URL; the token source is an open decision.
[[routes.claude.upstreams]]
id = "vertex-global"
provider = "vertex"
project = "acme-ml"
location = "global"
service_account_file = "vertex-sa.json"  # or a token_file an external refresher writes

# Agent-aware model routing within a provider: the allowlisted `model` rewrite.
[[routing.rules]]
request_class = "auxiliary"            # x-claude-code-request-class
model = "claude-haiku-5-5"

# The provable per-request cost ceiling (policy.md).
[policy]
max_request_cost_usd = "0.05"          # needs [pricing]
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

| Topic | Current preference / unresolved point |
|---|---|
| `include_usage` injection | Per-route; default undecided until client compatibility is tested |
| Configuration reload | Startup snapshot first; hot reload only on demand and without disrupting streams |
| SigV4 for AWS providers | Sign per request with an unsigned payload hash if Bedrock and Claude Platform on AWS accept it (to verify); otherwise support only their API keys |
| Vertex access tokens | Token exchange inside Wye (service-account key, RS256 JWT, one call per hour, in-memory cache) or an external refresher writing a token file; undecided |
| Edge support | WASM adapter after native correctness |
| Package registries | `wye` is taken on crates.io (as is `gateway-core`), so publishing there needs other names; releases ship as GitHub archives meanwhile |
