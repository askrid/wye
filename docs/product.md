# Product

Wye is an efficient, easy-to-use gateway between AI agents or AI-native apps and the model providers. Clients keep the provider's official SDK and native API, and get every new provider feature on the endpoints Wye serves the day it ships. Everything that is not application logic moves out of their code into one reviewed, Git-managed config: which model and provider serve a request, credentials, limits, failover, upgrades, telemetry, and cost. Wye does this per request and statelessly, as one binary with one TOML file and no database.

Primary users:

- Teams running coding agents (Claude Code first, Codex now through its native Responses API) who need service and CI tokens, reviewed policy, and visibility into what their agents do and spend.
- Teams building agents or AI-native apps on the provider APIs, who are tired of hard-coded model ids, per-model breaking changes, and the same plumbing (keys, retries, usage logging, cost) rebuilt in every service.

Individual developers can run the same setup locally.

## Motivation

A provider integration keeps needing work after it ships. Models arrive every few months and old ones retire, and each new model can reject what the previous one accepted: on Anthropic's API in one year, `thinking.budget_tokens`, sampling parameters, assistant prefill, forced `tool_choice`, and `output_format` all broke on newer models. Providers, accounts, and keys change with procurement and rate limits, and every service grows its own retries, logging, and cost tracking. The debt sits in application code, spread across repositories.

A gateway can absorb the part of that churn that is not application logic, but only if it stays transparent. A gateway that translates between provider APIs adds churn of its own, because its abstraction lags every new feature. Wye passes each native API through unchanged and adds indirection only where a decision belongs to operations: the model behind a name, the provider and account behind a model, the policy around a request.

Spend was part of the motivating case too: a team stopped using an AI gateway once model spend became hard to justify or control. Wye is designed to bound spend without a ledger of its own: per-request output-token ceilings today, and (planned) per-request cost ceilings and cumulative limits held by the provider that bills ([policy.md](policy.md#provider-held-budgets)).

Performance is a requirement: the gateway must never be the bottleneck, whether in streaming without buffering, memory per idle stream, CPU per event, or cancellation. Every commit is benchmarked on these against its parent ([development.md](development.md#commit-gate)). The results so far come from one laptop against a mock upstream; there are no production measurements yet ([research.md](research.md#current-harness-and-baseline)).

## Principles

Every feature follows these, and a feature that cannot is not built.

- **Native and transparent.** Each client's native API passes through to its own provider, and bodies are forwarded unchanged beyond the [allowlist](architecture.md#forwarding-contract). New provider features work through Wye without a Wye release. Telemetry carries ids, counts, and keyed hashes, never prompt text.
- **Report, don't rewrite.** Where a client's request will break (an upgrade, a policy), Wye refuses explicitly or reports it. It never silently changes what the client asked for.
- **Request-local and stateless.** A decision uses only the request, its token, and the immutable config; correlation across requests happens in the user's telemetry backend. Nothing authoritative carries across requests, and nothing moves between replicas ([policy.md](policy.md#state-boundary)).
- **Fast and light.** The request path reads headers; hashing, encoding, and aggregation happen in the telemetry writer task. A streamed event costs no allocation, and every queue and metric series has a fixed bound. Body scans are opt-in and sampled unless they are cheap enough to run on every request.
- **Easy to use.** Working defaults, so a local gateway needs a few config lines; `base_url` plus a token for any SDK; one command per agent (`wye claude`, `wye codex`); errors that say what to change; prebuilt binaries and images. A feature that needs a manual to set up is not done.
- **One responsibility.** Inference traffic only: no agent loop, tool execution, dashboard, or stored usage history.

## Scope

### Built

Detail and stages: [roadmap.md](roadmap.md#status).

- Native passthrough of Anthropic Messages (`/v1/messages`, `/v1/messages/count_tokens`), OpenAI Responses, and OpenAI Chat Completions. It honors Claude Code's [gateway contract](architecture.md#claude-code-gateway-contract) and serves the same calls from SDK clients, plus `GET /v1/models` answered from policy; batches, files, and other endpoints are not proxied. Streams are relayed as they arrive, with backpressure, cancellation, timeouts, a graceful two-phase shutdown, and bounds on concurrency and memory.
- Several Claude accounts per route, each session kept on one, with failover before output, across Anthropic, Claude Platform on AWS, and Bedrock with API keys ([design](architecture.md#claude-across-providers-m4)).
- Wye's own Ed25519 tokens (`wye token`), or identity-provider tokens verified directly (CI workload identity such as GitHub Actions, or the team's IdP through Claude Code's `apiKeyHelper`). Model permissions and output-token ceilings are checked before the provider is contacted.
- Caller-credential passthrough for a single developer, loopback only: a claude.ai login counts against the plan, and an API key bills the API.
- Model aliases and upgrade checks. Clients ask for a stable name (`coding-default`) that config maps to a model per team or token, so upgrading or retiring a model is a reviewed config change instead of edits across repositories. Before an alias moves, Wye checks live requests against the new model's known breaking changes and reports which clients would fail and why ([design](architecture.md#model-aliases-and-upgrade-checks)).
- A price table in config ([format](policy.md#pricing-metadata)) turns usage into dollars in records, metrics, and `wye report`.
- Agent-aware telemetry for Claude Code and Codex: usage records with subject, session, subagent, request class, prompt, compaction, and tool calls; OTLP logs, metrics, and traces, with a whole Claude Code session as one trace across replicas; tokens by work type; cache misses classified as expected or unexplained; opt-in keyed prompt hashes that explain a miss.
- Offline tools: `wye report` (a usage log summary without a backend), `wye explain` and `wye policy-test` (policy decisions checked in CI), `wye policy-diff` (fails CI when a config change widens access), `wye doctor`, `wye init`, `wye service`, and `wye probe`.
- Release archives for Linux and macOS built from a tag, and an OCI image built from the repository (not published).

### Next

In this order ([roadmap.md](roadmap.md)):

1. Verification against the real services: a real Codex session, one live Bedrock request, and the Claude Code exit check with an API key.
2. The rest of cost (M2): the provable per-request cost ceiling and provider-held budgets.
3. Packaging: a published container image and a Compose example.
4. The rest of Claude across providers (M4): Vertex AI and AWS SigV4.

### Later

Agent-aware model routing within a provider, a Helm chart, and other agents on their native APIs.

### Support

Status per client and provider: [roadmap.md](roadmap.md#status). A route counts as supported only once its request fields, tool-call identifiers and argument fragments, event ordering, finish reasons, errors, usage, and cancellation are tested. Where Wye cannot keep a request's semantics, it fails the request with an explicit error.

### Non-goals

- No translation between provider APIs and no unified cross-provider API: each client's native API goes to its own provider. Unified-API SDKs and LiteLLM own that space.
- No compatibility rewrites that hide breaking changes, such as dropping a parameter a new model rejects or converting `budget_tokens`: they silently change what the client asked for. Upgrade checks report instead.
- No dashboard, mutable admin API, authentication database, or migrations.
- No Redis, PostgreSQL, SQLite, distributed actors, leader election, peer discovery, or quota ownership.
- No exact global RPM/TPM, global concurrency cap, cumulative or monthly budget in Wye, or single-use token redemption.
- No MCP proxying, tool execution, or agent orchestration.
- No prompt compression or request rewriting beyond the [allowlist](architecture.md#forwarding-contract).
- No required cloud platform or observability vendor.

## Use cases

**Single developer.** `wye service install` and `wye claude`; `wye report` reads the usage log. [Caller-credential passthrough](policy.md#caller-credential-passthrough) keeps the developer's own login or API key, and the gateway holds no secret. The value here is modest, since Claude Code already exports its own usage; the point is that the developer later uses the same setup with a team.

**Engineering team with coding agents.** Developers and CI reach a shared Wye behind HTTPS. Provider keys stay in the deployment, callers hold gateway or IdP tokens, policy is reviewed in Git, and usage arrives in the team's OTel backend attributed by subject, session, subagent, and work type. Attribution comes from verified tokens, so a client cannot switch it off.

**Agents and AI-native apps.** Services call the provider SDK with `base_url` pointed at Wye and a gateway token. Model choice (through aliases), provider and account, limits, and failover live in `wye.toml`; usage, latency, and cost per service arrive without instrumentation, and upgrades are checked before they ship. A public app keeps its own user authentication, per-user quotas, and abuse controls in its backend: **Wye alone does not protect a public paid API from abuse**, because it keeps no state. Topologies: [operations.md](operations.md#deployment-modes).

## Positioning

The landscape as of 2026-10. Competitor features change quickly; re-check them before publishing any comparison.

| Alternative | Position | Where Wye differs |
|---|---|---|
| No gateway | SDK calls straight to the provider, keys per service, the provider console's workspaces, spend limits, and usage reports; agents' own telemetry | Keys, models, limits, and failover out of application code; one telemetry and cost model across clients; upgrade checks. For a few services on one provider, no gateway is enough |
| Claude apps gateway | Anthropic's self-hosted Claude Code gateway: SSO sign-in, model access by IdP group, OTLP | Service and CI tokens, SDK apps and other agents, aliases, multi-provider Claude, policy tests. A team that uses only Claude Code and has SSO should prefer it, since it tracks every Claude Code release by construction |
| LiteLLM | The common open-source gateway: unified API across many providers, virtual keys, budgets, fallbacks, admin UI; needs a database for spend | No database; native APIs passed through, so no feature lag; about 9× less CPU per streamed event in the same benchmark ([research.md](research.md#against-litellm)); far fewer providers by design |
| Newer lean gateways (Bifrost, Portkey, Helicone's gateway, and others) | Fast open-source gateways, several with dashboards, budgets, and many providers | Agent-aware attribution, stateless GitOps policy, upgrade checks. They compete on speed too; against Bifrost at its best case, Wye used 6.6× less CPU per request, added 11× less latency, and used 20× less memory under load ([research.md](research.md#against-bifrost)) |
| Hosted gateways (Cloudflare, Vercel, Kong, OpenRouter) | Managed, nothing to run | Self-hosted; the customer controls the path and the data |
| Observability tools (Langfuse, Arize, provider analytics) | Traces and analytics of LLM calls, through SDKs or exports | Captured at the gateway from verified tokens, without instrumentation; no replacement for evaluation tooling |

## Strengths and weaknesses

Strengths:

- Official SDKs and new provider features on the endpoints Wye serves work unchanged from day one.
- Agent-aware attribution, request-local and consistent across replicas.
- Trivial to operate on any platform that runs N identical processes behind a load balancer ([operations.md](operations.md#without-kubernetes)).
- CI and IdP tokens are verified directly against keys pinned in config, with no token service.
- Policy as code, checked offline.
- Measured, on a laptop against a mock upstream: about 0.2-0.3 ms added per request and no allocation per event, both checked on every commit, and about 1,500 token-paced streams on one core within 10 ms of added p99.

Weaknesses:

- Few providers so far, API keys only, with no Vertex yet, and Bedrock and Codex not yet checked live ([status](roadmap.md#status)).
- Not released: no version and no image is published yet. That hurts "easy to use" more than any missing feature.
- No cumulative limits, by design: budgets need the provider ([not built yet](policy.md#provider-held-budgets)), rate limits need infrastructure, and revocation waits for expiry.
- Little real-world validation ([status](roadmap.md#status)): one developer's Claude Code sessions on a subscription, no production deployment, and numbers from one laptop against a mock.
- Ongoing upkeep: Claude Code's gateway contract changes per release, and upgrade checks need a table of each model's breaking changes.

## Moat

Speed, statelessness, and single-binary distribution are necessary, and others can copy them. Two things build up over time: an understanding of agent traffic, turned into attribution and explanations, and a maintained record of each model's breaking changes, turned into upgrade checks so teams can upgrade models by config with evidence. Correctness proven for each client release and policy as code without a database add to that. Provider gateways can add the same analytics for their own clients, and lean gateways can add agent awareness. Neither serves several clients and providers behind one native-passthrough policy and one telemetry and cost model, with CI and service identities and the data in the team's own backend; that is where Wye can hold its ground.
