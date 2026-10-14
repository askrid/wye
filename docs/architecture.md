# Architecture

Start with one process and an Anthropic Messages streaming proxy for Claude Code. Preserve protocol behavior before adding routing or provider translation.

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
4. Resolve the model alias; check endpoint/feature/provider compatibility.
5. Select an eligible route by static policy and estimated cost.
6. Forward upstream, translating protocol only where necessary.
7. Stream incrementally with downstream backpressure; preserve tool calls and event semantics.
8. Propagate cancellation, apply the retry policy, release request resources.
9. Extract reported usage, or estimate it or mark it unknown; export telemetry asynchronously.

**Implemented** for Anthropic Messages: steps 1, 2, 3 (model permissions and output-token ceilings; cost ceilings are not built), 6–8, and 9 as usage records in a JSON-lines log and as OTLP logs, metrics, and spans. Not built: cost ceilings in step 3, and steps 4–5 (aliases and routing; one upstream today).

## Repository and workspace

One repository, four Cargo workspace members:

```text
wye/
├── crates/
│   ├── gateway-core/src/        portable policy, no I/O
│   │   ├── config.rs            wye.toml parsing and validation
│   │   ├── credential.rs        gateway credential verification and issuance
│   │   └── usage.rs             SSE usage scanner
│   ├── wye/src/                 the binary
│   │   ├── main.rs, token.rs    CLI, `wye token`
│   │   ├── startup.rs           secrets and files the config references
│   │   ├── server.rs            listener, connections, two-phase shutdown
│   │   ├── gateway.rs           routing, admission, gateway errors
│   │   ├── auth.rs              credential checks, cached per connection
│   │   ├── upstream.rs          forwarding, TLS client
│   │   ├── upstream/pool.rs     upstream connection lanes
│   │   ├── upstream/body.rs     response body: idle timeout, connection slot
│   │   ├── usage_log.rs         per-request records, bounded writer
│   │   ├── gauge.rs             in-flight counters released on drop
│   │   └── alloc_stats.rs       allocation counters (feature alloc-stats, benchmarks only)
│   ├── wye-testkit/             test CA and mock upstream
│   └── wye-bench/               benchmark, regression gate, competitor runs
├── bench/baseline.json
├── scripts/                     check.sh (commit gate), ab.sh
└── examples/wye.toml
```

Create modules only when their behavior is implemented.

- `gateway-core` owns configuration types, validation, credential policy, and usage parsing, and later routing. No Tokio, socket, filesystem, or environment dependencies, so it can later back a WASM adapter.
- `wye` owns the CLI, configuration loading, secret resolution, HTTP server and client, telemetry, and shutdown.
- `wye-testkit` and `wye-bench` are development-only; see [development.md](development.md).

Core, runtime, tests, and deployment artifacts change together and need coordinated review and release; multiple repositories would add versioning work without an ownership boundary. Do not add provider crates or a speculative WASM transport interface.

## Runtime stack

| Concern | Technology |
|---|---|
| Async execution | Tokio |
| HTTP | Hyper 1.x, `hyper-util`, `http-body-util` |
| TLS | Rustls (`ring` provider) upstream, built-in web PKI roots plus `upstream.ca_file`; deployment terminates inbound TLS |
| Credentials | `ring` Ed25519, `base64` |
| Config / JSON | Serde, `toml`, `serde_json` |
| Buffers | `bytes` and streaming bodies |
| Instrumentation | Usage records as a JSON-lines log on stderr and as OTLP logs OTLP metrics, and one OTLP span per request (hand-written OTLP/HTTP JSON exporter on the existing hyper client, instead of the OpenTelemetry Rust SDK and its gRPC stack) |
| Edge | Separate WASM adapter later |

Direct Hyper gives explicit control over request and response bodies. Reconsider Axum if routing and middleware grow enough to justify it.

A request may run tens of seconds with little CPU. The important costs are per-connection buffers, HTTP/2 multiplexing, TLS, memory per task, fairness, slow-client backpressure, and cancellation. Never block executor threads with synchronous work.

Upstream connections come from one pool of lanes ([upstream connections](#upstream-connections)). Immutable configuration, connection pools, and in-flight request state are compatible with statelessness; persistent conversations, global quotas, and cumulative spend accounting are not.

## Public API

- `POST /v1/messages`, `POST /v1/messages/count_tokens`: Anthropic Messages, native passthrough to Anthropic (P0, Claude Code).
- `POST /v1/chat/completions`: Chat Completions with streaming and client-managed tool calls.
- `POST /v1/responses`: Responses, initially only where backed by the native OpenAI Responses API.
- `GET /healthz`, `GET /readyz`: operational probes. `GET /v1/models` only when a client integration requires it (Claude Code model discovery is opt-in).

An `ANTHROPIC_BASE_URL` or `OPENAI_BASE_URL` override works only when the client respects it **and** uses an implemented endpoint. Base-URL compatibility is not compatibility with proprietary SDK extensions.

### Claude Code gateway contract

Claude Code documents what a gateway must do ([gateway compatibility guide](https://code.claude.com/docs/en/llm-gateway-protocol)). P0 acceptance tests encode it:

- Match on path, not full URL: inference posts to `/v1/messages?beta=true`; forward the query string.
- Forward all `anthropic-*` request headers and all body fields unchanged, as open lists. Never allowlist `anthropic-beta` values: capabilities pair a beta header with a body field, and stripping one half produces `400`s on the next Claude Code release.
- Forward `retry-after`, `x-should-retry`, and `anthropic-ratelimit-unified-*` response headers, and upstream error bodies unmodified; Claude Code's retry and capability-recovery logic matches upstream error wording.
- Relay every SSE event, including `ping`. Claude Code aborts an idle stream and treats a stream that ends before `message_delta`/`message_stop` as dropped.
- Forward the `system` array unchanged; its first block is an attribution block the Anthropic API strips by position.
- Consume, never require, `x-claude-code-session-id`, `x-claude-code-agent-id`, and gateway hint headers such as `x-claude-code-request-class` for telemetry attribution and, later, [agent-aware routing](research.md#1-agent-aware-routing). A custom base URL receives hint headers only when developers set `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`.
- Answer the best-effort `HEAD /api/hello` warm-up probe with an empty `200` before authentication; it carries no data and reaches no upstream.
- `/v1/messages/count_tokens` is unbilled: it requires authentication and model permission but skips cost admission.
- Gateway-generated errors use Anthropic's error JSON shape and status codes, so Claude Code's retry and recovery logic reads them as it reads upstream errors.
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

**Bodies.** Preserve request JSON, unknown fields, tool definitions, and provider-native options. The request body streams to the upstream unchanged, bounded by `server.max_request_body_bytes` (default 32 MiB, Anthropic's limit). With [model permissions](policy.md#gateway-credentials), only the start of the body is held, until its top-level `model` has been read; the held bytes are bounded across the process by `server.max_held_request_bytes`, and the body is never re-serialized. Never buffer the generated response. Relay SSE bytes without parsing or rebuilding events; HTTP chunks are not SSE event boundaries. Preserve non-streaming JSON and upstream error bodies.

**Request rewrites.** Wye rewrites only an explicit allowlist of request fields; everything else passes through byte-equivalent in meaning:

| Field | When | Why |
|---|---|---|
| `model` | Alias resolution (M4+) | Replace the alias with the selected upstream model |
| `max_tokens` / `max_completion_tokens` / `max_output_tokens` | Hard cost mode | Insert the output bound used in the [cost ceiling](policy.md#provable-cost-ceiling) when absent; lower a caller value only when the route explicitly accepts truncation, otherwise reject; never raise a caller value. On Messages, never lower below `thinking.budget_tokens`; reject instead |
| `stream_options.include_usage` | Streaming Chat Completions with usage accounting | OpenAI omits streamed usage otherwise ([usage extraction](#usage-extraction)) |

Each rewrite is per-route configuration, covered by tests, and reported by `wye explain`. Adding a field to this list is a design change.

### Usage extraction

Telemetry and cost accounting need usage, which arrives in the final response object or the last stream events. Wye relays the original bytes unchanged and feeds the same bytes to a bounded, read-only observer that recognizes only usage-bearing events. The observer never delays, buffers, or modifies forwarded bytes. It caps the size of an event it will inspect; an oversized or malformed event marks usage `unknown` rather than failing the stream.

Implemented for Anthropic Messages streams (`gateway-core/src/usage.rs`): the scanner keeps at most one partial line (32 bytes for ordinary events, 64 KiB for usage events), finds line ends with SIMD `memchr`, and parses JSON only for `message_start` and `message_delta`. A body wrapper emits one record per request when the body is dropped, so completed, failed, cancelled, and gateway-error outcomes are all recorded; completion is also detected through `is_end_stream`, because hyper stops polling a known-length body without seeing its end. Non-streaming responses are recorded with `usage_status = not_observed` until a bounded JSON reader is added.

Anthropic Messages streams always carry usage (`message_start` for input and cache tokens, `message_delta` for output), so P0 needs no rewrite. For OpenAI Chat Completions, enabling `include_usage` makes OpenAI send an extra final chunk with an empty `choices` array. Most SDKs accept it, but a client that indexes `choices[0]` unconditionally breaks. If the caller already set `include_usage`, Wye leaves it as is. Otherwise the route decides: inject it (`accounting_status = confirmed`), or don't (`accounting_status = estimated`, from the [cost ceiling](policy.md#provable-cost-ceiling) bound). Disconnected streams record usage as `unknown` with the known bound.

**Headers and destination.** Strip hop-by-hop headers, including those named by `Connection`. Replace caller authorization (`Authorization` or `x-api-key`) with the configured provider credential, except in explicit loopback [caller-credential passthrough](policy.md#caller-credential-passthrough). Forward only appropriate end-to-end headers. The caller never selects the upstream destination (no open proxy or SSRF surface). Preserve the configured base-path prefix.

**Backpressure and cancellation.** Downstream body polling applies backpressure. No unbounded queues or detached readers. Client disconnect releases or cancels upstream work, verified by integration tests. Already generated tokens may still be billed.

**Errors.** Before downstream headers are sent, local failures produce a gateway error in the client-facing API's error shape (Anthropic error JSON on `/v1/messages`). Upstream error bodies pass through unmodified. After headers are sent, an upstream failure terminates the stream; it cannot become a new JSON error response.

**Timeouts and shutdown.** Use three timeouts: a short connect timeout; a response-header timeout of several minutes, because a non-streaming generation sends no headers until it completes (Claude Code falls back to non-streaming requests after a stream failure); and a streaming-inactivity timeout longer than the provider's `ping` interval. No total request timeout: it would kill valid generations. Shutdown has a drain phase (readiness fails, requests still served) and a grace phase that cancels whatever remains ([operations.md](operations.md#health-and-shutdown)).

## Retries and fallback

The initial proxy does not retry or fall back: an ambiguous failure may already have incurred charges, and switching providers after output starts cannot preserve the response. On `/v1/messages`, Wye never retries: Claude Code retries on its own, guided by the `x-should-retry` and `retry-after` headers Wye forwards, and gateway retries would multiply attempts and risk duplicate billing. When retries are added for other routes, they follow this policy:

| Failure stage | Policy |
|---|---|
| Local validation, before upstream send | No upstream work; return error or choose another eligible route |
| Connect failure before the request reaches the provider | Retry may be safe within a bounded deadline |
| Upstream may have accepted the request, no response yet | Retry risks duplicate billing; conservative |
| Output already sent downstream | Never transparently replay from another model |
| Partial tool-call payload emitted | No automatic fallback without higher-level recovery semantics |

Keep request IDs and trace linkage. Do not claim exactly-once execution.

## Providers and adapters

Start with a concrete Anthropic upstream module, not a trait with one implementation. When a second provider lands, use an explicit provider enum and provider-specific modules; extract shared interfaces only around demonstrated shared behavior.

**Routing** chooses provider and model before sending. **Protocol adaptation** translates requests, responses, errors, usage, and streaming events. Keep them separate.

Each adapter declares exactly which endpoint and features it supports:

- Roles, messages, and multimodal content forms.
- Tool schemas, `tool_choice`, tool call IDs, streamed arguments and results.
- Structured output options and refusal semantics.
- Streaming event transformation and finish reasons.
- Usage accounting and provider caching fields.
- Authentication, API versions, deadlines, error classification.
- Provider-hosted tools, including hosted MCP, only when implemented and tested.

### Upstream connections

hyper-util's client pool keeps a single HTTP/2 connection per host and never opens another, so requests beyond the provider's concurrent-stream limit would queue inside it. `upstream/pool.rs` keeps 64 lanes, each a client with its own pool and therefore its own connection, created at startup and connected lazily. Each request takes a slot on the least loaded lane (ties to the earliest), held by the response body until the stream ends: sequential traffic reuses one connection, `k` concurrent requests use up to `k` connections, and beyond 64 the load stays even. Spreading is deliberate: `h2` guards all streams of one connection with one mutex, and packing streams onto a connection made that lock the dominant cost of the TLS path.

### Upstream-held state

Some requests reference state stored by a specific provider account: Responses `previous_response_id` (with `store=true`), stored conversations, uploaded file IDs, and provider cache entries. Wye stores none of it, but routing must respect it:

- A request that references upstream-held state is pinned to the provider and credential that can resolve it, or rejected with an explicit error. It is never routed elsewhere.
- Determining that provider and credential must be stateless: from the route configuration (a route with a single eligible account) or the gateway credential. If neither identifies it, reject.

### Cache-affine routing

Provider prompt caches are per account (and sometimes per region), and cached input is priced at a large discount. Cost routing that spreads one conversation across accounts or providers forfeits that discount.

When a route has several eligible candidates, Wye can choose deterministically by rendezvous hashing over the candidates, keyed by the credential subject plus a hash of a stable request prefix (system prompt and tool definitions). The same workload then reaches the same candidate from any replica, with no shared state, and moves only when the candidate set changes. Strategy name: `cache_affine` (illustrative). Its cost benefit is a hypothesis to measure ([research.md](research.md#questions-to-test)).

Pure affinity can overload one candidate when a few prefixes dominate traffic, a capacity loss recent serving research reports ([research.md](research.md#2-cache-miss-diagnostics)). The strategy therefore needs a load bound that stays stateless: a static share per candidate in config (consistent hashing with bounded loads applied to configured weights), or a placement plan fitted offline from telemetry and committed to config. Every replica still computes the same choice.

Prefer passthrough where native compatibility is safe; translate only for heterogeneous providers. If semantics cannot be preserved, reject with a documented incompatibility. Responses semantics are treated separately from Chat Completions. Passing provider-native options through does not mean Wye interprets or enforces them.

### Tools and MCP

```text
Agent ──tools/schema──> Wye ──> model
Agent <─tool_calls───── Wye <── model
Agent ──executes tool / uses MCP server independently
Agent ──tool results─> Wye ──> model
```

This pattern needs no MCP gateway. Provider-hosted MCP/tools require explicit passthrough or translation support.


## Configuration

One TOML file, loaded at startup, is the source of truth for aliases, providers, routes, credential verification, and limits. Secrets are referenced by environment variable, never stored in Git, images, or IaC state.

The runtime resolves environment variables; the core validates configuration without resolving secrets. Reject unknown fields, invalid URLs or limits, broken references, duplicate IDs, unsupported provider/API pairs, missing credentials, and missing price metadata at startup. Never print secrets or log request bodies by default.

No hot reload initially; changes require restart (or rolling deploy). A config snapshot stays consistent within a request. Include the config version in logs and traces to diagnose rollout skew.

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

[upstream]
base_url = "https://api.anthropic.com" # http only for loopback hosts
api_key_env = "ANTHROPIC_API_KEY"      # or: api_key_file = "/run/secrets/anthropic" (relative to this file)
                                       # or: forward_caller_credential = true (loopback, no [auth])
# ca_file = "corporate-ca.pem"         # extra trusted roots, relative to this file
connect_timeout_secs = 10
response_header_timeout_secs = 600
stream_idle_timeout_secs = 120

[auth]                                 # optional
issuer = "wye"
audience = "wye-prod"
max_credential_ttl_secs = 604800
[[auth.keys]]
id = "k1"
ed25519_public_key = "<from wye token keygen>"
# models = ["claude-sonnet-*", "claude-haiku-*"]  # ceiling; a credential's models claim narrows it
# max_output_tokens = 32000            # max_tokens ceiling; a credential's claim can only lower it

[telemetry]                            # optional: usage records as OTLP logs
otlp_endpoint = "http://collector:4318" # POST {otlp_endpoint}/v1/logs, OTLP/HTTP JSON
# otlp_headers_env = "OTLP_HEADERS"    # extra headers, "key=value,key2=value2"
service_name = "wye"

[cache_diagnostics]                    # optional: prompt hashes in the usage log
hash_key_env = "WYE_CACHE_HASH_KEY"    # required; at least 32 bytes, same on every replica
sample_percent = 100                   # share of Claude Code sessions diagnosed
```

### Target schema (illustrative, not implemented)

```toml
[server]
listen = "0.0.0.0:4000"
shutdown_grace_seconds = 120

[providers.openai]
kind = "openai"
api_key_env = "OPENAI_API_KEY"

[providers.anthropic]
kind = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"

[models.claude_messages]
provider = "anthropic"
upstream_model = "REPLACE_WITH_SUPPORTED_ANTHROPIC_MODEL"
capabilities = ["messages", "messages_stream", "tool_calls"]

[models.openai_chat]
provider = "openai"
upstream_model = "REPLACE_WITH_SUPPORTED_OPENAI_MODEL"
capabilities = ["chat", "chat_stream", "tool_calls"]

[models.openai_responses]
provider = "openai"
upstream_model = "REPLACE_WITH_RESPONSES_MODEL"
capabilities = ["responses", "responses_stream", "tool_calls"]

[models.anthropic_chat]
provider = "anthropic"
upstream_model = "REPLACE_WITH_SUPPORTED_ANTHROPIC_MODEL"
capabilities = ["chat", "chat_stream", "tool_calls"]

[routes.claude_code]
strategy = "first_eligible"
candidates = ["claude_messages"]
required_capabilities = ["messages_stream", "tool_calls"]
on_no_eligible_candidate = "reject"

[routes.fast]
strategy = "cache_affine"
candidates = ["openai_chat", "anthropic_chat"]
required_capabilities = ["chat_stream"]
on_no_eligible_candidate = "reject"

[routes.codex]
strategy = "first_eligible"
candidates = ["openai_responses"]
required_capabilities = ["responses_stream", "tool_calls"]
on_no_eligible_candidate = "reject"

[auth]
issuer = "wye"
audience = "wye-prod"
[[auth.keys]]
id = "k1"
ed25519_public_key = "<base64url Ed25519 public key>"

[policy]
max_output_tokens = 4096
max_request_cost_usd = "0.05"
unknown_cost_behavior = "reject"

[telemetry]
protocol = "otlp"
endpoint_env = "OTEL_EXPORTER_OTLP_ENDPOINT"
log_prompt_content = false
```

`claude_code` is a Messages route backed only by Anthropic, `codex` is a Responses route backed only by OpenAI, and `fast` is a Chat Completions route; none is interchangeable with another. The implementation should pick a schema where aliases declare or infer their endpoint unambiguously.

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
| HTTP framework | Simplest correct Hyper implementation; Axum if routing grows |
| Workspace layout | Four crates above; split further only as responsibilities stabilize |
| Token format | Decided: compact JWT with `alg` pinned to `EdDSA` ([policy.md](policy.md#gateway-credentials)); Macaroons/Biscuit only if delegation is needed |
| Token issuance | Implemented: `wye token issue` offline; external issuers producing the same format work too; the server holds verification keys only |
| `include_usage` injection | Per-route; default undecided until client compatibility is tested |
| Caller-credential passthrough | Explicit, loopback only; no gateway auth or cost ceiling in that mode ([policy.md](policy.md#caller-credential-passthrough)) |
| Price estimates | Versioned table plus conservative estimation; define guarantee tiers |
| Configuration reload | Startup snapshot first; hot reload only on demand and without disrupting streams |
| `/v1/models` | Optional; implement if a client integration requires it |
| Responses cross-provider translation | Not part of the initial multi-provider promise |
| Provider-hosted tools/MCP | Explicit adapter feature, not generic passthrough |
| Per-node adaptive concurrency | Violates strict statelessness; research only |
| Dynamic provider health routing | Excluded unless expressed as static policy |
| Edge support | WASM adapter after native correctness |
| Maximum stream/request lifetime | Configurable timeouts plus shutdown semantics |
| License / package name | After package, domain, and trademark checks |
