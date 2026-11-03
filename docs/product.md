# Product

Provider keys, models, failover, limits, and cost for Claude Code, Codex, and your apps live in one TOML file in Git, and each provider's native API passes through without translation.

Wye is a gateway between AI agents or AI-native apps and the model providers. Clients keep the provider's official SDK and native API, and get every new provider feature on the endpoints Wye serves the day it ships. Everything that is not application logic moves out of their code into one reviewed, Git-managed config: which model and provider serve a request, provider keys, limits, failover, upgrades, telemetry, and cost. Wye does this per request and statelessly, as one binary with one TOML file and no database.

Primary users:

- Teams running coding agents (Claude Code first, Codex now through its native Responses API) who need service and CI tokens, reviewed policy, and visibility into what their agents do and spend.
- Teams building agents or AI-native apps on the provider APIs, who are tired of hard-coded model ids, per-model breaking changes, and the same plumbing (provider keys, retries, usage logging, cost) rebuilt in every service.

Individual developers can run the same setup locally.

## Motivation

A provider integration keeps needing work after it ships. Models arrive every few months and old ones retire, and each new model can reject what the previous one accepted: on Anthropic's API in one year, `thinking.budget_tokens`, sampling parameters, assistant prefill, forced `tool_choice`, and `output_format` all broke on newer models. Providers, accounts, and API keys change with procurement and rate limits, and every service grows its own retries, logging, and cost tracking. The debt sits in application code, spread across repositories.

A gateway can absorb the part of that churn that is not application logic, but only if it stays transparent. A gateway that translates between provider APIs adds churn of its own, because its abstraction lags every new feature. Wye passes each native API through unchanged and adds indirection only where a decision belongs to operations: the model behind a name, the [upstream](glossary.md) (provider and account) behind a model, the policy around a request.

Spend was part of the motivating case too: a team stopped using an AI gateway once model spend became hard to justify or control. Wye is designed to bound spend without a ledger of its own: per-request output-token ceilings today, and (planned) per-request cost ceilings and cumulative limits held by the provider that bills ([policy.md](policy.md#provider-held-budgets)).

Performance is a requirement, measured rather than claimed: streaming without buffering, memory per idle stream, CPU per event, and cancellation. Every commit that changes code is benchmarked on these against its parent ([development.md](development.md#commit-gate)). The results so far come from one laptop against a mock upstream; there are no production measurements yet ([benchmarks.md](benchmarks.md#current-harness-and-baseline)).

## Principles

Every feature follows these, and a feature that cannot is not built.

- **Native and transparent.** Each client's native API passes through to its own provider, and bodies are forwarded unchanged beyond the [allowlist](architecture.md#forwarding-contract). New provider features work through Wye without a Wye release. Telemetry carries ids, counts, and keyed hashes, never prompt text.
- **Report, don't rewrite.** Where a client's request will break (an upgrade, a policy), Wye refuses explicitly or reports it. It never silently changes what the client asked for: the one field it changes, the model, changes only where reviewed config says so (an alias, a provider's own model id such as Bedrock's, or, planned, a routing rule the operator turns on), and never out of sight, since the response names the model that ran and the usage record keeps both.
- **Request-local and stateless.** A decision uses only the request, its [gateway token](glossary.md), and the immutable config; correlation across requests happens in the user's [OpenTelemetry backend](glossary.md). Nothing authoritative carries across requests, and nothing moves between replicas ([policy.md](policy.md#state-boundary)).
- **Fast and light.** The request path reads headers, and the body only where a policy or an opt-in scan needs it; keying, encoding, and aggregation happen in the telemetry writer task. A streamed event allocates nothing, and every queue and metric series has a fixed bound. Body scans are opt-in and sampled unless they are cheap enough to run on every request.
- **Little to set up.** Working defaults, so a local gateway needs a few config lines; `base_url` plus a token for any SDK; one command per agent (`wye claude`, `wye codex`); errors that say what to change; prebuilt binaries and images. A feature that needs a manual to set up is not done.
- **One responsibility.** Inference traffic only: no agent loop, tool execution, dashboard, or stored usage history.

## Scope

### Built

What Wye does today: [README](../README.md#what-it-does); status per milestone, and what is not yet checked against the live services: [roadmap.md](roadmap.md#status).

Endpoints: Anthropic Messages (`/v1/messages`, `/v1/messages/count_tokens`), OpenAI Responses, OpenAI Chat Completions, and `GET /v1/models` answered from config. It honors Claude Code's [gateway contract](architecture.md#claude-code-gateway-contract) and serves the same calls from SDK clients. Batches, files, and other endpoints are not proxied.

### Next

In this order ([roadmap.md](roadmap.md)):

1. Fewer expensive tokens (M2c): the waste report in `wye report`, size-aware model routing for small background requests, and an estimate of what Wye saved.
2. Spend caps per team, held by the provider (M2): each team's gateway tokens use its own Anthropic workspace key, whose spend limit Anthropic enforces ([design](policy.md#provider-held-budgets)).
3. Verification against the real services: a real Codex session, one live request each on Bedrock and Claude Platform on AWS, and the Claude Code exit check with an API key.
4. The rest of cost (M2): the provable per-request cost ceiling.
5. Packaging: a published container image and a Compose example.
6. Narrowable tokens: a token holder hands a subagent or a job a narrower token without the issuer ([research](research.md#3-narrowable-tokens-for-agent-swarms)).
7. The rest of Claude across providers (M4): Vertex AI and AWS SigV4.

### Later

Wider agent-aware model routing (by subagent type, beyond size), a Helm chart, and other agents on their native APIs.

### Support

Status per client and provider: [roadmap.md](roadmap.md#status). An endpoint counts as supported only once its request fields, tool-call identifiers and argument fragments, event ordering, finish reasons, errors, usage, and cancellation are tested. Where Wye cannot keep a request's semantics, it fails the request with an explicit error.

### Non-goals

- No translation between provider APIs and no unified cross-provider API: each client's native API goes to its own provider. Unified-API SDKs and LiteLLM own that space.
- No compatibility rewrites that hide breaking changes, such as dropping a parameter a new model rejects or converting `budget_tokens`: they silently change what the client asked for. Upgrade checks report instead.
- No dashboard, mutable admin API, authentication database, or migrations.
- No Redis, PostgreSQL, SQLite, distributed actors, leader election, peer discovery, or quota ownership.
- No exact global RPM/TPM, global concurrency cap, cumulative or monthly budget in Wye, or single-use token redemption.
- No circuit breakers, cooldowns, or balancing by measured latency: they remember across requests. Failover handles a [rejection](glossary.md) within the request.
- No swapping in another model when one fails, no content filtering (guardrails, redaction), and no stored prompts.
- No TLS termination: the load balancer, ingress, or Caddy in front does it and renews the certificate.
- No MCP proxying, tool execution, or agent orchestration.
- No prompt compression or request rewriting beyond the [allowlist](architecture.md#forwarding-contract).
- No required cloud platform or observability vendor.

## Use cases

**Single developer.** `wye service install` and `wye claude`; `wye report` reads the usage log. [Caller-credential passthrough](policy.md#caller-credential-passthrough) keeps the developer's own login or API key, and the gateway holds no secret. The value today is the breakdown Claude Code's own totals lack: by session, subagent, and tool, with cache misses flagged. The waste report (next, [roadmap](roadmap.md#m2c-fewer-expensive-tokens)) is meant to make it worth installing on its own.

**Engineering team with coding agents.** Developers and CI reach a shared Wye behind HTTPS. Provider keys stay in the deployment, callers hold gateway tokens from Wye or the team's identity provider, the config is reviewed in Git, and usage arrives in the team's OTel backend attributed by subject, session, subagent, and [request class](glossary.md). Attribution comes from verified gateway tokens, so a client cannot switch it off.

**Agents and AI-native apps.** Services call the provider SDK with `base_url` pointed at Wye and a gateway token. Model choice (through aliases), upstreams, limits, and failover live in `wye.toml`; usage, latency, and cost per service arrive without instrumentation, and upgrades are checked before they ship. A public app keeps its own user authentication, per-user quotas, and abuse controls in its backend: **Wye alone does not protect a public paid API from abuse**, because it keeps no state. Topologies: [operations.md](operations.md#deployment-modes).

## Positioning

The landscape as of 2026-10. Competitor features change quickly; re-check them before publishing any comparison.

- **No gateway**: SDK calls straight to the provider, API keys per service, the provider console's workspaces, spend limits, and usage reports; agents' own telemetry. Where Wye differs: provider keys, models, limits, and failover out of application code; one telemetry and cost model across clients; upgrade checks. For a few services on one provider, no gateway is enough.
- **Claude apps gateway**: Anthropic's self-hosted Claude Code gateway, with SSO sign-in, model access by IdP group, and OTLP. Where Wye differs: service and CI tokens, SDK apps and other agents, aliases, multi-provider Claude, policy tests. A team that uses only Claude Code and has SSO should prefer it, since it tracks every Claude Code release by construction.
- **LiteLLM**: the common open-source gateway, with a unified API across many providers, virtual keys, budgets, fallbacks, and an admin UI; it needs a database for spend. Where Wye differs: no database; native APIs passed through, so no feature lag; about 9× less CPU per streamed event in the same benchmark ([benchmarks.md](benchmarks.md#against-litellm)); far fewer providers by design.
- **Newer lean gateways** (agentgateway, Bifrost, Portkey, Helicone's gateway, and others): fast open-source gateways, several with dashboards, budgets, and many providers. Where Wye differs: agent-aware attribution, stateless GitOps policy, upgrade checks. They compete on speed too; in each comparison on the same machine, Wye used less CPU per request ([performance](../README.md#performance)).
- **Hosted gateways** (Cloudflare, Vercel, Kong, OpenRouter): managed, nothing to run. Where Wye differs: self-hosted; the customer controls the path and the data.
- **Observability tools** (Langfuse, Arize, provider analytics): traces and analytics of LLM calls, through SDKs or exports. Where Wye differs: captured at the gateway from verified gateway tokens, without instrumentation; no replacement for evaluation tooling.

## Strengths and weaknesses

Strengths:

- Official SDKs and new provider features on the endpoints Wye serves work unchanged from day one.
- Agent-aware attribution, request-local and consistent across replicas.
- Trivial to operate on any platform that runs N identical processes behind a load balancer ([operations.md](operations.md#without-kubernetes)).
- CI and other identity-provider tokens are verified directly against keys pinned in config, with no token service.
- Policy as code, checked offline.
- Measured, on a laptop against a mock upstream: about 0.2-0.3 ms added per request and no allocation per event, both checked on every commit that changes code, and about 1,500 token-paced streams on one core within 10 ms of added p99.

Weaknesses:

- Few providers so far, API keys only, with no Vertex yet, and Bedrock and Codex not yet checked live ([status](roadmap.md#status)).
- Prerelease only: `0.1.0-rc.1` archives exist, but no container image is published. That costs "little to set up" more than any missing feature.
- No cumulative limits, by design: budgets need the provider ([planned next after M2c](policy.md#provider-held-budgets)), rate limits need infrastructure, and revocation waits for expiry.
- Little real-world validation ([status](roadmap.md#status)): one developer's Claude Code sessions on a subscription, no production deployment, and numbers from one laptop against a mock.
- Ongoing upkeep: Claude Code's gateway contract changes per release, and upgrade checks need a table of each model's breaking changes.
