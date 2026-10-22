# Wye design docs

Wye is the gateway that understands agent traffic: a stateless Rust inference gateway for coding agents such as Claude Code. One binary and one Git-managed TOML file; no database, no control plane. It authenticates and constrains requests, streams them, reads the signals agents send (session, subagent, request class, compaction, tool time) to attribute and explain their work, and exports it through OpenTelemetry. Scope, strengths, and weaknesses: [product.md](product.md).

**Status (2026-10-22):** built for Claude Code on the Anthropic Messages API: the streaming proxy (M0), credentials including identity-provider tokens, model permissions and token ceilings, OTLP telemetry, `wye claude`, and a local OCI image (M1), and agent-aware visibility (M1b; secret detection open). Next: cost (M2). Not started: Codex (M3), Claude across providers and accounts (M4, designed), published packages. Detail: [roadmap.md](roadmap.md#status).

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
| [development.md](development.md) | Code layout, commit gate, benchmark, contribution rules |

## Conventions

- **Requirement:** intended design decision. Change only with an explicit design review.
- **Proposal / illustrative:** not yet implemented; shape may change.
- **Research:** idea to investigate, not a product promise.
- Do not claim performance improvements without published, reproducible benchmarks.
- Support is declared per **endpoint × provider × feature**. Do not imply more.
