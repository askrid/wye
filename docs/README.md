# Wye design docs

Wye is an efficient, easy-to-use gateway between AI agents or AI-native apps and the model providers, in Rust. Clients keep the provider's official SDK and native API; model choice, provider, credentials, limits, failover, upgrades, telemetry, and cost live in one Git-managed TOML file. One stateless binary, no database, no control plane. It reads the signals coding agents send (session, subagent, request class, compaction, tool time) to attribute and explain their work, and exports it through OpenTelemetry. Scope, strengths, and weaknesses: [product.md](product.md).

Status: built for Claude Code and Anthropic SDK clients (Anthropic, Claude Platform on AWS, Bedrock) and for Codex and OpenAI SDK clients (OpenAI); Claude Code sessions run through it on a subscription since 2026-10-30, not yet with an API key. What is built and what comes next: [product.md](product.md#scope); stages: [roadmap.md](roadmap.md#status).

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
