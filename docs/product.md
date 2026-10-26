# Product

**Wye is an efficient, easy-to-use gateway between AI agents or AI-native apps and the model providers.** Clients keep the provider's official SDK and native API, and get every new provider feature the day it ships. Everything that is not application logic moves out of their code into one reviewed, Git-managed config: which model and provider serve a request, credentials, limits, failover, upgrades, telemetry, and cost. Wye does this per request, statelessly, at a cost the client never notices: one binary, one TOML file, no database.

**Primary users:**

- **Teams running coding agents** (Claude Code first, Codex next) who need service and CI credentials, reviewed policy, and visibility into what their agents do and spend.
- **Teams building agents or AI-native apps** on the provider APIs, who are tired of hard-coded model ids, per-model breaking changes, and the same plumbing (keys, retries, usage logging, cost) rebuilt in every service.

Secondary: individual developers, with the same setup locally.

## Motivation

Integrating a model provider is not a one-time job. Models arrive every few months and old ones retire; each new model can reject what the previous one accepted (on Anthropic's API in one year: `thinking.budget_tokens`, sampling parameters, assistant prefill, forced `tool_choice`, and `output_format` all broke on newer models); providers, accounts, and keys change with procurement and rate limits; and every service grows its own retries, logging, and cost tracking. The debt sits in application code, spread across repositories.

A gateway can absorb the part of that churn that is not application logic, but only if it stays transparent. A gateway that translates between provider APIs adds a second source of churn: its abstraction lags every new feature. Wye passes each native API through unchanged and puts indirection only where a decision belongs to operations: the model behind a name, the provider and account behind a model, the policy around a request.

The motivating case also included spend: a team stopped using an AI gateway once model spend became hard to justify or control. Wye bounds spend without a ledger of its own: per-request ceilings, and cumulative limits held by the provider that bills ([policy.md](policy.md#provider-held-budgets)).

**Performance is a requirement, not the pitch.** The gateway must never be the bottleneck: streaming without buffering, memory per idle stream, CPU per event, cancellation. Every commit is gated on these ([development.md](development.md#commit-gate)). Results so far come from one laptop against a mock upstream and are lab results, not production claims ([research.md](research.md#current-harness-and-baseline)).

## Principles

Every feature obeys these; a feature that cannot is not built.

- **Native and transparent.** Each client's native API passes through to its own provider; bodies are forwarded unchanged beyond the [allowlist](architecture.md#forwarding-contract). New provider features work through Wye without a Wye release. Telemetry carries ids, counts, and keyed hashes, never prompt text.
- **Report, don't rewrite.** Where a client's request will break (an upgrade, a policy), Wye refuses explicitly or reports it; it never silently changes what the client asked for.
- **Request-local and stateless.** A decision uses only the request, its credential, and the immutable config; correlation across requests happens in the user's telemetry backend. No authoritative cross-request state; nothing moves between replicas ([policy.md](policy.md#state-boundary)).
- **Fast and light.** Header reads on the request path; hashing, encoding, and aggregation in the telemetry writer task. Zero allocations per streamed event. Every queue and metric series has a fixed bound. Body scans are opt-in and sampled unless they fit the gate on their own.
- **Easy to use.** Working defaults, so a local gateway needs a few config lines; `base_url` plus a credential for any SDK; one command per agent (`wye claude`, later `wye codex`); errors that say what to change; prebuilt binaries and images. A feature that needs a manual to set up is not done.
- **One responsibility.** Inference traffic only: no agent loop, tool execution, dashboard, or stored usage history.

## Scope

### Built

For the Anthropic Messages API, passthrough to Anthropic ([roadmap.md](roadmap.md#status) has the detail):

- **Streaming proxy** for `/v1/messages` and `/v1/messages/count_tokens`, honoring Claude Code's [gateway contract](architecture.md#claude-code-gateway-contract) and serving the same calls from Anthropic SDK clients (batches, files, and other endpoints are not proxied): SSE relayed as it arrives, backpressure, cancellation, timeouts, graceful two-phase shutdown, concurrency and memory bounds.
- **Credentials:** Wye's own Ed25519 tokens (`wye token`), or identity-provider tokens directly (CI workload identity such as GitHub Actions, or the team's IdP through Claude Code's `apiKeyHelper`), with model permissions and output-token ceilings checked before the provider is contacted.
- **Subscription passthrough** for a single developer on a claude.ai plan, loopback only.
- **Agent-aware telemetry:** usage records with subject, session, subagent, request class, prompt, compaction, and tool calls; OTLP logs, metrics, and traces, with a whole Claude Code session as one trace across replicas; tokens by work type; cache misses classified as expected or unexplained; opt-in keyed prompt hashes that explain a miss.
- **Offline tools:** `wye report` (usage log summary, no backend), `wye explain` and `wye policy-test` (policy decisions checked in CI), `wye probe`, `GET /v1/models` from policy.
- **Deployment:** native binary from source, OCI image (built locally, not published).

### Next

In this order ([roadmap.md](roadmap.md)):

1. **Cost (M2):** pricing in config ([format](policy.md#pricing-metadata)), so usage becomes dollars in records, metrics, and `wye report` (built); next, the provable per-request cost ceiling and provider-held budgets.
2. **Packaging:** published binaries and images, a Compose example.
3. **Model aliases and upgrade checks (M2b):** clients ask for a stable name (`coding-default`) that config maps to a model, per team or credential, so a model upgrade or retirement is a reviewed config change instead of edits across repositories. Before an alias moves to a new model, Wye checks live requests against that model's known breaking changes and reports which clients would fail and why (built; [design](architecture.md#model-aliases-and-upgrade-checks)).
4. **Claude across providers and accounts (M4):** one name served from Anthropic, Claude Platform on AWS, Amazon Bedrock, Google Vertex AI, and several accounts, chosen per request with cache affinity, failing over on overload before any output. Clients write no Bedrock or Vertex code; moving traffic is a config change ([design](architecture.md#claude-across-providers-planned-m4)).

### Later

More clients on their native APIs: Codex through the OpenAI Responses API (M3), Chat Completions passthrough for OpenAI SDK clients, then other agents. The cost bound and provider-held budgets (rest of M2), agent-aware model routing, Helm.

| Phase | Clients | Client-facing API | Upstream | Status |
|---|---|---|---|---|
| P0 | Claude Code, Anthropic SDK apps and agents | Anthropic Messages | Anthropic | **Built** |
| P1 | Codex | OpenAI Responses | OpenAI | Planned (M3) |
| P2 | OpenAI SDK apps and agents | OpenAI Chat Completions | OpenAI | Planned (M3) |
| P3 | Same as P0, across providers | Anthropic Messages | Anthropic, Claude Platform on AWS, Bedrock, Vertex | Planned (M4) |
| Later | Other agents (for example Gemini CLI) | Their native API | Their provider | Research |

A route is supported only when its request fields, tool-call identifiers and argument fragments, event ordering, finish reasons, errors, usage, and cancellation are tested. **Fail explicitly rather than silently drop semantics.**

### Non-goals

- No translation between provider APIs and no unified cross-provider API: each client's native API goes to its own provider. Unified-API SDKs and LiteLLM own that space.
- No compatibility rewrites that hide breaking changes (dropping a parameter a new model rejects, converting `budget_tokens`): they silently change what the client asked for. Upgrade checks report instead.
- No dashboard, mutable admin API, authentication database, or migrations.
- No Redis, PostgreSQL, SQLite, distributed actors, leader election, peer discovery, or quota ownership.
- No exact global RPM/TPM, global concurrency cap, cumulative or monthly budget in Wye, or single-use token redemption.
- No MCP proxying, tool execution, or agent orchestration.
- No prompt compression or request rewriting beyond the [allowlist](architecture.md#forwarding-contract).
- No required cloud platform or observability vendor.

## Use cases

**Single developer.** `wye serve` locally and `wye claude`; `wye report` reads the usage log. On a claude.ai subscription, [caller-credential passthrough](policy.md#caller-credential-passthrough) keeps the developer's own login. Honest value here is modest: Claude Code already exports its own usage. The case is the same setup the developer later uses with a team.

**Engineering team with coding agents.** Developers and CI reach a shared Wye behind HTTPS; provider keys stay in the deployment, callers hold gateway or IdP credentials, policy is reviewed in Git, and usage arrives in the team's OTel backend attributed by subject, session, subagent, and work type. Attribution comes from verified credentials, so it cannot be switched off client-side.

**Agents and AI-native apps.** Services call the provider SDK with `base_url` pointed at Wye and a gateway credential. Model choice (through aliases), provider and account, limits, and failover live in `wye.toml`; usage, latency, and cost per service arrive without instrumentation; upgrades are checked before they ship. A public app keeps its own user authentication, per-user quotas, and abuse controls in its backend: **stateless Wye alone is not abuse protection for a public paid API.** Topologies: [operations.md](operations.md#deployment-modes).

## Positioning

Landscape as of 2026-10; competitor features change quickly and should be re-checked before any comparison is published.

| Alternative | Position | Where Wye differs |
|---|---|---|
| No gateway | SDK calls straight to the provider, keys per service, the provider console's workspaces, spend limits, and usage reports; agents' own telemetry | Keys, models, limits, and failover out of application code; one telemetry and cost model across clients; upgrade checks. The default to beat: for a few services on one provider it is enough |
| Claude apps gateway | Anthropic's self-hosted Claude Code gateway: SSO sign-in, model access by IdP group, OTLP | Service and CI credentials, SDK apps and other agents, aliases, multi-provider Claude, policy tests. **For a team that uses only Claude Code and has SSO, prefer it:** it tracks every Claude Code release by construction |
| LiteLLM | The common open-source gateway: unified API across many providers, virtual keys, budgets, fallbacks, admin UI; needs a database for spend | No database; native APIs passed through, so no feature lag; about 10× less CPU per streamed event in the same benchmark ([research.md](research.md#against-litellm)); far fewer providers by design |
| Newer lean gateways (Bifrost, Portkey, Helicone's gateway, and others) | Fast, open-source gateways, several with dashboards, budgets, and many providers | Agent-aware attribution, stateless GitOps policy, upgrade checks; they compete on Wye's own speed claim. Measured against Bifrost at its best case: 6.6× less CPU per request, 11× lower added latency, 20× less memory under load ([research.md](research.md#against-bifrost)) |
| Hosted gateways (Cloudflare, Vercel, Kong, OpenRouter) | Managed, nothing to run | Self-hosted, customer-controlled path and data |
| Observability tools (Langfuse, Arize, provider analytics) | Traces and analytics of LLM calls, through SDKs or exports | Captured at the gateway from verified credentials, no instrumentation; not a replacement for evaluation tooling |

## Strengths and weaknesses

**Strengths**

- **Native passthrough:** official SDKs and new provider features work unchanged, day one.
- **Agent-aware attribution:** sessions, subagents, work types, tool time, and explained cache misses, request-local and consistent across replicas.
- **Operationally trivial:** one stateless binary and one TOML file on any platform that runs N identical processes behind a load balancer ([operations.md](operations.md#without-kubernetes)).
- **Identity without a token service:** CI and IdP tokens verified directly, with keys pinned in config.
- **Policy as code:** `explain` and `policy-test` show and check every decision offline; credentials can only narrow config.
- **Measured efficiency, gated on every commit:** about 0.2–0.3 ms added per request, zero allocations per event, about 1,500 token-paced streams within 10 ms added p99 on one core (laptop, mock upstream).

**Weaknesses**

- **One API, one upstream so far.** Anthropic Messages to one Anthropic account; no OpenAI, no multi-provider Claude, no failover yet.
- **Not packaged.** Build from source; no published binaries or images. This works against "easy to use" more than any missing feature.
- **No cumulative limits by design:** budgets need the provider ([not built yet](policy.md#provider-held-budgets)), rate limits need infrastructure, and revocation waits for expiry.
- **Thin real-world validation.** One partial real Claude Code session; no production deployment; numbers from one laptop against a mock.
- **Upkeep it takes on:** Claude Code's gateway contract per release, and (with upgrade checks) a table of each model's breaking changes.

## Moat

Speed, statelessness, and single-binary distribution are necessary but copyable. What compounds: understanding of agent traffic turned into attribution and explanations; a maintained knowledge of each model's breaking changes turned into upgrade checks, so teams upgrade models by config with evidence; correctness proven per client release; and policy as code with no database. The risks are plain: provider gateways can add the same analytics for their own clients, and lean gateways can add agent awareness. Wye's defensible ground is what neither serves: several clients and providers behind one native-passthrough policy and one telemetry and cost model, CI and service identities, and data in the team's own backend.
