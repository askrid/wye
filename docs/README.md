# Wye docs

New here: the [README](../README.md#what-it-does) says what Wye does, the [glossary](glossary.md) defines its terms, [how it compares](../README.md#how-it-compares) with what teams expect of a gateway, and how fast it is, and the [guide](guide.md) shows how to use each part. What Wye is for and why: [product.md](product.md). Each document owns one concern; others link to it instead of restating it.

## Use it

| Document | Answers |
| --- | --- |
| [guide.md](guide.md) | Choosing a setup, running it, reading `wye report`, teams, apps, several keys or providers, costs, model aliases and upgrades, an OTel backend, changing or upgrading, troubleshooting, FAQ |
| [examples/wye.toml](../examples/wye.toml) | Every configuration field: its default, limits, and meaning (parsed by a test with every section enabled) |
| [CHANGELOG.md](../CHANGELOG.md) | What each release contains and what it leaves unverified |

## Run it

| Document | Answers |
| --- | --- |
| [operations.md](operations.md) | Distribution, CLI, deployment topologies, telemetry, health, shutdown, failure behavior |

## Trust it

| Document | Answers |
| --- | --- |
| [benchmarks.md](benchmarks.md) | What Wye costs, the method, competitors, the gate's evidence |
| [policy.md](policy.md) | What Wye authorizes and enforces, what it cannot enforce without shared state, security requirements |
| [architecture.md](architecture.md) | How a request flows: components, runtime, forwarding, providers, streaming, configuration, invariants, open design decisions |

## Change it

| Document | Answers |
| --- | --- |
| [development.md](development.md) | Commit gate, CI, benchmark scenarios, releases, design principles, review rounds |
| [roadmap.md](roadmap.md) | Status, open work, exit criteria, open findings, release checklists |
| [research.md](research.md) | Facts to verify, related work, feature directions, where the tokens go, visibility for one developer, performance techniques |
| [product.md](product.md) | Why Wye exists, its principles, scope and build order, use cases, alternatives |

## Terms

[glossary.md](glossary.md) defines each term the docs, the CLI, and the code use, and the names retired for it.

## Conventions

- **Proposal / illustrative:** not yet implemented; shape may change.
- **Research:** idea to investigate, not a product promise.
- These docs and the README are the source of truth for design, status, and results. A change updates the docs it affects in the same commit: status in [roadmap.md](roadmap.md), the config reference in [examples/wye.toml](../examples/wye.toml), structure in [architecture.md](architecture.md), numbers in [benchmarks.md](benchmarks.md), experiments in [research.md](research.md).
- Do not claim performance improvements without published, reproducible benchmarks, and never present an estimate or a lab result as a production measurement.
- After editing docs, check that every relative link and anchor resolves.
- Support is declared per **endpoint × provider × feature**. Do not imply more.
- **Terms:** one name per concept, as [glossary.md](glossary.md) defines it; a new concept gets its entry there first.
- **Layout:** one line per paragraph and per list item (editors and GitHub wrap them). Tables hold short values; prose goes in lists, so GitHub does not squeeze it into narrow columns. Code lines stay under 90 characters.
