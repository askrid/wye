# Wye design docs

**Status:** M0 (Claude Code streaming proxy) and M1 stage 4a (gateway credentials) are implemented; see [roadmap.md](roadmap.md#status). Sections marked as proposal or target describe design that is not built yet.

Wye is a single-purpose, stateless Rust inference gateway. It authenticates, constrains, routes, and streams LLM requests using one declarative configuration file, and exports usage and operational telemetry through OpenTelemetry. It requires no gateway-specific datastore or administrative control plane.

## Documents

Each document owns one concern. Other documents link to it instead of restating it.

| Document | Answers |
| --- | --- |
| [product.md](product.md) | Why Wye exists, who uses it, scope, provider/endpoint matrix, non-goals, positioning |
| [architecture.md](architecture.md) | How a request flows: components, runtime, forwarding, adapters, streaming, configuration, invariants, open design decisions |
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
