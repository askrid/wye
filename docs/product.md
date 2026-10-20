# Product

**Wye is the gateway that understands agent traffic.** Coding agents such as Claude Code send long, cache-heavy, bursty sessions made of a main loop, subagents, background calls, and compactions. Generic gateways see a stream of unrelated requests. Wye reads the signals agents already send to attribute, explain, and constrain that work per request, and stays fast, light, and stateless while doing it: one binary, one Git-managed TOML file, no database.

**Primary users:** engineering teams running coding agents (Claude Code first, Codex next) who need service credentials, reviewed policy, and visibility into what their agents spend and why. Secondary: individual agent developers, and applications serving LLM-backed features.

## Motivation

An engineering team used an AI gateway and then stopped because model spending became hard to justify or control. It still wanted centralized credentials, one interface to models, and usage attribution, with little infrastructure to run. Wye provides those without a ledger of its own: per-request ceilings bound each request, and cumulative spend is capped where it is billed, at the provider ([policy.md](policy.md#provider-held-budgets)).

**Performance is a requirement, not the pitch.** When inference takes seconds, saving proxy milliseconds rarely matters; what matters is that the gateway is never the bottleneck: streaming without buffering, memory per idle stream, CPU per event, cancellation. Every commit is gated on these ([development.md](development.md#commit-gate)). Results so far come from one laptop against a mock upstream and are lab results, not production claims ([research.md](research.md#current-harness-and-baseline)).

## Principles

Every feature obeys these; a feature that cannot is not built.

- **Request-local.** A decision uses only the request, its credential, and the immutable config. Correlation across requests (sessions, prompts, subagents) happens in the user's telemetry backend through ids every replica derives alike, never in Wye.
- **Stateless.** No authoritative cross-request state: sockets, pools, buffers, the config snapshot, and bounded telemetry queues only. Nothing needs to move between replicas ([policy.md](policy.md#state-boundary)).
- **Fast and light.** Header reads on the request path; hashing, encoding, and aggregation in the telemetry writer task. Zero allocations per streamed event. Every queue and metric series has a fixed bound and drops and counts on overflow. Body scans are opt-in and sampled per session unless they fit the gate on their own.
- **Transparent.** Bodies are forwarded unchanged beyond the [allowlist](architecture.md#forwarding-contract). Telemetry carries ids, counts, and keyed hashes, never prompt text.
- **One responsibility.** Inference traffic only: no agent loop, tool execution, dashboard, or stored usage history. `wye claude` launching a client is a client-side convenience.

## Scope

### Built

For Claude Code on the Anthropic Messages API, passthrough to Anthropic ([roadmap.md](roadmap.md#status) has the detail):

- **Streaming proxy** honoring Claude Code's [gateway contract](architecture.md#claude-code-gateway-contract): SSE relayed as it arrives, backpressure, cancellation, timeouts, graceful two-phase shutdown, concurrency and memory bounds.
- **Credentials:** Wye's own Ed25519 tokens (`wye token`), or identity-provider tokens directly (CI workload identity such as GitHub Actions, or the team's IdP through Claude Code's `apiKeyHelper`), with model permissions and output-token ceilings checked before the provider is contacted.
- **Subscription passthrough** for a single developer on a claude.ai plan, loopback only.
- **Agent-aware telemetry:** usage records with session, subagent, request class, prompt, compaction, and tool calls; OTLP logs, metrics, and traces, with a whole Claude Code session as one trace across replicas; tokens by work type; cache misses classified as expected or unexplained; opt-in keyed prompt hashes that explain a miss.
- **Offline tools:** `wye report` (usage log summary, no backend), `wye explain` and `wye policy-test` (policy decisions checked in CI), `wye probe`, `GET /v1/models` from policy.
- **Deployment:** native binary from source, OCI image (built locally, not published).

### Next (M2: cost)

Pricing metadata, so usage becomes dollars in records, metrics, and `wye report`; a provable per-request cost bound as a runaway guard; provider-held budgets (identity → provider workspace key). This is the gap between "what our agents used" and "what they cost".

### Later

Codex through the OpenAI Responses API (M3), then multi-provider routing with Chat Completions adapters (M4), published binaries and images, Helm and Compose, an edge adapter. Phases per endpoint and provider:

| Phase | Client-facing API | Upstream | Status | Notes |
|---|---|---|---|---|
| P0 | Anthropic Messages | Anthropic | **Built** | Claude Code; native passthrough |
| P1 | Chat Completions | OpenAI | Planned | Native passthrough |
| P2 | Responses | OpenAI | Planned | Codex; native event fidelity |
| P3 | Chat Completions | Anthropic | Planned | Translation; reject unsupported semantics explicitly |
| P4 | Chat Completions | Gemini | Planned | Feature-by-feature test matrix |
| Later | Anthropic Messages | Claude on Bedrock / Vertex | Research | Claude models only |

A route is supported only when request fields, tool-call identifiers and argument fragments, event ordering, finish reasons, errors, usage, cancellation, and unsupported-field handling are tested. **Fail explicitly rather than silently drop semantics.** Chat Completions support alone is never Codex compatibility; Codex needs the Responses route.

**Open question:** P3 and P4 (translating between providers) are the largest planned work and the area where LiteLLM's breadth is strongest. They serve applications more than coding agents. Whether they stay in scope, or Wye stays a passthrough gateway for agents (Claude Code, Codex) and leaves translation to others, is undecided.

### Non-goals

- No dashboard, mutable admin API, authentication database, or migrations.
- No Redis, PostgreSQL, SQLite, distributed actors, leader election, peer discovery, or quota ownership.
- No exact global RPM/TPM, global concurrency cap, cumulative or monthly budget in Wye, or single-use token redemption.
- No MCP proxying, tool execution, or agent orchestration.
- No prompt compression or request rewriting beyond the [allowlist](architecture.md#forwarding-contract).
- No generic API gateway for non-LLM APIs; no assumed equivalence across models or providers.
- No required cloud platform or observability vendor.

## Use cases

**Single developer.** `wye serve` locally and `wye claude`; no server or backend needed, and `wye report` reads the usage log. On a claude.ai subscription, [caller-credential passthrough](policy.md#caller-credential-passthrough) keeps the developer's own login. Honest value here is modest: Claude Code already exports its own usage, and passthrough adds no authentication or cost bound. The case is the same setup the developer later uses with a team.

**Engineering team.** Developers and CI reach a shared Wye behind HTTPS; provider keys stay in the deployment, callers hold gateway or IdP credentials, policy is reviewed in Git, and usage arrives in the team's OTel backend attributed by subject, session, subagent, and work type. Wye records usage from verified credentials, so attribution cannot be switched off client-side. Hard team budgets belong to the provider or the infrastructure.

**Public LLM app.** Wye sits behind the app backend, which authenticates users and owns per-user quotas and abuse controls; Wye authenticates the app and applies inference policy. **Stateless Wye alone is not abuse protection for a public paid API.** Topologies: [operations.md](operations.md#deployment-modes).

## Positioning

| Product | Position | Where Wye differs |
|---|---|---|
| Claude apps gateway | Anthropic's self-hosted Claude Code gateway, shipped with the `claude` binary: SSO sign-in, model access by IdP group, OTLP | Service and CI credentials, clients beyond Claude Code (SDK apps, Codex planned), per-request guards, policy tests, session traces and work-type attribution. **For a team that uses only Claude Code and has SSO, prefer it:** it tracks every Claude Code release by construction |
| LiteLLM | Broad provider coverage, virtual keys, budgets, admin UI; needs a database for spend | No database; about 10× less CPU per streamed event in the same benchmark ([research.md](research.md#against-litellm)); far narrower provider coverage |
| KrakenD | Mature general API gateway | CE buffers whole responses (measured); SSE streaming is Enterprise-only |
| OmniRoute, Vercel AI Gateway | Multi-account routing and quotas; managed convenience | Self-hosted, GitOps policy, protocol-focused scope |

## Strengths and weaknesses

**Strengths**

- **Agent-aware attribution no generic gateway has:** sessions, subagents, work types, tool time, and explained cache misses, request-local and consistent across replicas.
- **Operationally trivial:** one stateless binary and one TOML file; any platform that runs N identical processes behind a load balancer ([operations.md](operations.md#without-kubernetes)).
- **Identity without a token service:** CI and IdP tokens verified directly, with keys pinned in config.
- **Policy as code:** `explain` and `policy-test` show and check every decision offline; credentials can only narrow config.
- **Measured efficiency, gated on every commit:** about 0.2–0.3 ms added per request, zero allocations per event, about 1,500 token-paced streams within 10 ms added p99 on one core (laptop, mock upstream).

**Weaknesses**

- **One endpoint, one provider.** Only Anthropic Messages to one Anthropic upstream; no aliases, routing, OpenAI, or Gemini yet.
- **Tokens, not dollars.** No pricing yet, so no cost in reports and no cost ceiling (M2).
- **No cumulative limits by design:** budgets need the provider ([provider-held budgets](policy.md#provider-held-budgets), not built yet) and rate limits need infrastructure. Revocation waits for expiry.
- **Thin real-world validation.** One partial real Claude Code session; no production deployment; numbers from one laptop against a mock, none on Linux or across machines.
- **Not packaged.** Build from source; no published binaries or images, no Helm or Compose.
- **Contract upkeep.** Claude Code's gateway contract changes with its releases, and Wye must re-test each one, which Anthropic's own gateway does not.

## Moat

Speed, statelessness, and single-binary distribution are necessary but copyable. What compounds is understanding of agent traffic (sessions, subagents, compactions, caching) turned into attribution and explanations, correctness proven per client release, and policy as code with provable properties and no database. The risk is plain: Anthropic's gateway can add the same analytics for Claude Code. Wye's defensible ground is what a single-vendor gateway does not serve: several agents and SDK apps behind one policy, CI and service identities, and telemetry in the team's own backend.
