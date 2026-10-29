# Wye design docs

Wye is an efficient, easy-to-use gateway between AI agents or AI-native apps and the model providers, in Rust. Clients keep the provider's official SDK and native API; model choice, provider, credentials, limits, failover, upgrades, telemetry, and cost live in one Git-managed TOML file. One stateless binary, no database, no control plane. It reads the signals coding agents send (session, subagent, request class, compaction, tool time) to attribute and explain their work, and exports it through OpenTelemetry. Scope, strengths, and weaknesses: [product.md](product.md).

**Status (2026-10-27):** built for the Anthropic Messages API (Claude Code and Anthropic SDK clients): the streaming proxy (M0), credentials including identity-provider tokens, model permissions and output-token ceilings, OTLP telemetry, `wye claude`, and a local OCI image (M1), agent-aware visibility (M1b; secret detection open), model aliases with per-subject rollout and upgrade checks (M2b), cost in usage records from a price table (M2 pricing), several accounts per route with session affinity and failover before output, Claude Platform on AWS and Bedrock with API keys (M4 stages 10–12), OpenAI Responses and Chat Completions passthrough with `wye codex` (M3), usage and cost for streaming and non-streaming responses on all three APIs, `wye init`, `wye doctor`, and `wye policy-diff`. Next, in order ([product.md](product.md#next)): verification against the real services (a Codex session, a Bedrock request, the Claude Code exit check), the cost ceiling and provider-held budgets (rest of M2), a published image, and Vertex and SigV4 (rest of M4). Detail: [roadmap.md](roadmap.md#status).

## Documents

Each document owns one concern. Other documents link to it instead of restating it.

| Document | Answers |
| --- | --- |
| [product.md](product.md) | Why Wye exists, who uses it, scope (built, next, later), non-goals, positioning, strengths and weaknesses, moat |
| [architecture.md](architecture.md) | How a request flows: components, runtime, forwarding, providers, streaming, configuration, invariants, open design decisions |
| [policy.md](policy.md) | What Wye authorizes and enforces, what it cannot enforce without shared state, security requirements |
| [operations.md](operations.md) | Distribution, CLI, deployment topologies, telemetry, health, shutdown, failure behavior |
| [roadmap.md](roadmap.md) | Milestones, implementation stages, tests, acceptance criteria |
| [research.md](research.md) | Open questions, measurement methodology, benchmark results, research directions |
| [development.md](development.md) | Code layout, commit gate, CI, benchmark, releases, contribution rules |
| [examples/wye.toml](../examples/wye.toml) | Every configuration field: its default, limits, and meaning (parsed by a test with every section enabled) |
| [CHANGELOG.md](../CHANGELOG.md) | What each release contains and what it leaves unverified |

## Conventions

- **Requirement:** intended design decision. Change only with an explicit design review.
- **Proposal / illustrative:** not yet implemented; shape may change.
- **Research:** idea to investigate, not a product promise.
- Do not claim performance improvements without published, reproducible benchmarks.
- Support is declared per **endpoint × provider × feature**. Do not imply more.
