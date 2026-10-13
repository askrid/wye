# Product

**Primary users:** individual agent developers, engineering teams, and applications serving LLM-backed functionality.

## Motivation

An engineering team used an AI gateway and then stopped because model API spending became difficult to justify or control, shifting toward reimbursement. The team still benefits from centralized credentials, a consistent model interface, and usage attribution. Wye should provide those benefits with little infrastructure and operational work.

Wye answers that problem without a ledger of its own: a **provable per-request cost ceiling** bounds each request, and **provider-held budgets** cap cumulative spend at the provider, which already keeps the authoritative ledger ([policy.md](policy.md#provable-cost-ceiling)).

Wye does not reproduce a managed AI platform. Its intended advantages:

1. **Low operational burden:** one process per instance, one Git-managed config, no Redis, SQL database, dashboard, or management service.
2. **Easy adoption:** applications redirect their SDK `base_url`; coding agents launch through a CLI wrapper.
3. **Inference-specific efficiency:** streaming is the normal path; runtime work and buffering stay small.
4. **Transparent cost decisions:** routing, per-request constraints, versioned pricing, and usage attribution are explicit and testable.
5. **Portability:** inference policy is independent of the deployment platform, from a local subprocess to a container or a compatible edge runtime.

**Performance is a hypothesis, not a claimed advantage.** Go and mature proxies are already efficient, and when inference takes seconds, saving a few proxy milliseconds rarely matters. The relevant measures are memory per active stream, CPU per streamed byte, added time to first token (TTFT), cancellation, and infrastructure cost. Measured so far on one laptop against a mock upstream: Wye adds about 0.2–0.3 ms per request and streams events as they arrive, where KrakenD CE buffers whole responses and LiteLLM adds tens of milliseconds ([research.md](research.md#against-krakend)). These are lab results, not production claims.

### One process, one responsibility

The gateway service handles LLM inference traffic only. It does not run an agent loop, execute tools, host a dashboard, provision infrastructure, or store global usage history. A CLI wrapper may launch another tool such as Codex; that is a client-side convenience and does not make the gateway a multi-process control plane.

### Stateless means no authoritative cross-request state

A running process has sockets, request-scoped futures, connection pools, buffers, immutable config, and bounded telemetry queues. None of these is authoritative user or quota state, and none needs to move to another replica. Wye must not depend on the history of prior requests to authorize or route a new one. Consequences are in [policy.md](policy.md#state-boundary).

## Scope

### Core functionality

- OpenAI-compatible `POST /v1/chat/completions` and `POST /v1/responses`, in the phased combinations below.
- Model aliases and static, declarative routing between eligible provider/model targets.
- SSE streaming with cancellation propagation, bounded buffers, and backpressure.
- Transparent client-managed tool calls; no tool execution.
- Signed gateway credentials with expiry, audience, identity, and model permissions.
- Per-request input/output limits and conservative cost admission when billable cost can be bounded.
- Provider-native prompt caching where supported; no gateway response cache.
- Usage, latency, and cost telemetry, traces, and structured errors via OTLP.
- Native binary, OCI image, local CLI, then Helm and deployment examples; edge adapters later.

### Provider and endpoint phases

Two independent dimensions: the **client-facing endpoint** (Chat Completions or Responses) and the **upstream provider**. Support is declared and tested per endpoint × provider × feature. There is no universal interchangeability layer.

| Phase | Client-facing API | Upstream | Streaming | Client-managed tool calls | Notes |
|---|---|---|---|---|---|
| P0 | Anthropic Messages | Anthropic | Yes | Preserve and test | Claude Code-focused; native passthrough |
| P1 | Chat Completions | OpenAI | Yes | Preserve and test | Native passthrough |
| P2 | Responses | OpenAI | Yes | Preserve and test | Codex-focused; native event fidelity |
| P3 | Chat Completions | Anthropic | Yes | Translate supported forms | Reject unsupported semantics explicitly. First multi-provider release |
| P4 | Chat Completions | Gemini | Yes | Translate supported forms | Feature-by-feature test matrix |
| Later | Anthropic Messages | Claude on Bedrock / Vertex | TBD | TBD | Claude models only: Anthropic does not support Claude Code on non-Claude models |
| Later | Responses | Anthropic / Gemini | TBD | TBD | Separate capability; no promise until adapters are correct |
| Later | Other provider-native APIs | Matching provider | TBD | TBD | Optional fidelity-oriented path |

A route is supported only when request fields, tool-call identifiers and argument fragments, event ordering, finish reasons, errors, usage, cancellation, and unsupported-field handling are tested. **Fail explicitly rather than silently drop semantics.**

Claude Code requires the Messages route and its [gateway contract](architecture.md#claude-code-gateway-contract). Chat Completions support alone is never sufficient Codex compatibility; Codex requires the Responses route. Milestones are in [roadmap.md](roadmap.md).

### Non-goals

- No dashboard, mutable admin API, authentication database, or migrations.
- No Redis, PostgreSQL, SQLite, distributed actor system, leader election, peer discovery, or quota ownership.
- No exact global RPM/TPM, global concurrency cap, cumulative or monthly budget, or single-use token redemption.
- No MCP discovery, MCP transport proxy, managed tool execution, or agent orchestration.
- No built-in prompt compression or request rewriting beyond the [allowlist](architecture.md#forwarding-contract).
- No generic API aggregation or KrakenD replacement for non-LLM APIs.
- No assumed semantic equivalence across models or providers.
- No required cloud platform or observability vendor.

## Use cases

### Single developer and coding agents

```text
Claude Code / Agent ──> Wye (localhost) ──> LLM provider
```

```bash
wye claude
wye --profile personal claude
```

No server, orchestrator, state store, or telemetry backend is mandatory. Provider secrets stay in the developer's environment or OS credential store. A developer on a claude.ai subscription can keep it: in explicit loopback [caller-credential passthrough](policy.md#caller-credential-passthrough) Wye forwards the developer's own login instead of a provider key. The gateway starts and stops with the tool. Loopback-only development authentication must be explicitly opted into and must never extend silently to a public bind address.

Value for a solo developer alone is modest: Claude Code already exports its own usage and cost through OpenTelemetry, and in subscription passthrough Wye adds neither authentication nor a cost bound. The honest pitch is the same local setup the developer will use with their team, plus one config across clients, and a runaway guard on API-key traffic. This user usually needs spending awareness, not hard monthly stops.

### Engineering team

```text
Developer A ─┐
Developer B ─┼──> HTTPS endpoint / internal service ──> Wye ──> providers
CI / agents ─┘                                       └──> OTLP backend
```

Provider credentials stay in the team's deployment environment; applications receive only gateway credentials. Routing policy is reviewed in Git. Usage is recorded by Wye from verified credentials, so it is trustworthy attribution that a developer cannot switch off or alter the way client-side telemetry can. OTLP events attribute usage by application, project, or key identity without collecting prompts by default. Wye does not provide hard team budgets; use application or infrastructure enforcement if required.

### Public internet-facing LLM app

```text
Browser ──> App backend (auth / abuse controls / user quotas)
                          │
                          v
                   Wye (internal/sidecar)
                          │
                          v
                      Provider
```

Users are untrusted. Never issue a reusable provider key to the browser. The application backend authenticates users and owns per-user quotas, fraud controls, and admission; Wye authenticates the application and applies inference policy. **Stateless Wye alone is not sufficient cost-abuse protection for a public paid API.** Topology options are in [operations.md](operations.md#deployment-modes).

## Positioning

| Product | General position | Why Wye might be selected |
|---|---|---|
| KrakenD | Mature general API gateway; AI features vary by edition | Streams SSE as it arrives; KrakenD CE buffers whole responses (measured), streaming is Enterprise-only |
| LiteLLM | Broad provider coverage, virtual keys, budgets, management | No database to run; about 10× less CPU per streamed event and far less memory in the same benchmark ([research.md](research.md#against-litellm)) |
| OmniRoute | Multi-account routing, quotas, compression, management | Deterministic GitOps policy, lean serving path, protocol-focused scope |
| Vercel AI Gateway | Managed, convenient within Vercel | Self-hosted, cross-platform, customer-controlled path |
| Claude apps gateway | Anthropic's self-hosted Claude Code gateway, shipped in the `claude` binary and released with it: SSO sign-in, model access by IdP group, OTLP | CI and service credentials (it has only a browser sign-in), clients beyond Claude Code (SDK apps, Codex), per-request runaway guards, GitOps policy tests. **For a team that uses only Claude Code and already has SSO, prefer it:** it tracks every Claude Code release by construction, while Wye must re-run its contract tests for each one |

Stateless deployment and single-binary distribution are not unique. Wye must win by combining correct LLM compatibility, predictable configuration, portability, and measured resource efficiency.

### Differentiators

Each follows from statelessness rather than working around it:

| Differentiator | What it gives | Where |
|---|---|---|
| Provable cost ceiling | A guaranteed per-request dollar bound with no tokenizer, instead of an estimate | [policy.md](policy.md#provable-cost-ceiling) |
| Provider-held budgets | Hard cumulative spend limits with zero gateway state | [policy.md](policy.md#provider-held-budgets) |
| Cache-affine routing | Keeps provider prompt-cache discounts across replicas without shared state | [architecture.md](architecture.md#cache-affine-routing) |
| Policy as testable code | `wye explain` and CI policy tests: routing and cost decisions reviewed like code | [operations.md](operations.md#server) |
| Test-generated support matrix | Every claimed endpoint × provider × feature is backed by a passing test | [roadmap.md](roadmap.md#release-acceptance-checklist) |
| Performance gated on every commit | Added latency, CPU, and memory measured against a baseline before each commit; published, reproducible comparisons | [development.md](development.md#commit-gate) |

Planned differentiators that use what only an agent-aware gateway sees, still request-local and stateless (detail in [research.md](research.md#feature-directions)):

| Direction | What it gives |
|---|---|
| Agent-aware routing | Claude Code's request-class hints route auxiliary and compaction calls to cheaper Claude models by policy |
| Cache-miss diagnostics | Per-block prompt hashes let the telemetry backend explain every prompt-cache miss |
| Delegable credentials | Agents mint narrower credentials for subagents offline |
| Signed usage receipts | Tamper-evident chargeback and audit without a billing database |
| Queryable policy | "Who can reach which model, at what cost?" answered from config, checked in CI |

### Moat

Speed and statelessness are necessary but copyable. What compounds: correctness for agent traffic, proven per client release by contract tests; performance enforced on every commit and published reproducibly; policy as code with provable properties and no database; and the agent-aware directions above. The target niche is teams running several coding agents who need service credentials and reviewed policy, which neither a single-vendor gateway nor a stateful platform serves well.
