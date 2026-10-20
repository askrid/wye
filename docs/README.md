# Wye design docs

**Status:** M0 (Claude Code streaming proxy), most of M1 (credentials, model permissions, token ceilings, OTLP telemetry, `wye claude`, OCI image), and most of M1b (agent-aware visibility: session traces, spend by work type, cache-miss reasons, `wye report`) are implemented; see [roadmap.md](roadmap.md#status). Sections marked as proposal or target describe design that is not built yet.

Wye is the gateway that understands agent traffic: a single-purpose, stateless Rust inference gateway for coding agents such as Claude Code. It authenticates, constrains, routes, and streams LLM requests using one declarative configuration file, reads the signals agents send (session, subagent, request class, compaction) to attribute and explain their work, and exports it through OpenTelemetry. It stays fast, light, and stateless: no gateway-specific datastore, no administrative control plane, and every feature measured against the commit gate ([product.md](product.md#agent-native-within-the-constraints)).

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
