# Wye docs

New here: the [README](../README.md#what-it-does) says what Wye does, [how it compares](../README.md#how-it-compares) with what teams expect of a gateway, and how fast it is, and the [guide](guide.md) shows how to use each part. What Wye is for and why: [product.md](product.md). Each document owns one concern; others link to it instead of restating it.

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

| Term | Meaning |
| --- | --- |
| Route | The upstreams that serve one native API: `[routes.claude]` (or its one-account short form, `[upstream]`) for Anthropic Messages, `[routes.openai]` for OpenAI Responses and Chat Completions |
| Upstream | One provider account a route sends requests to: an `id`, a `provider`, a base URL, and its key |
| Provider | Who serves an upstream: `anthropic`, `claude-platform-aws`, `bedrock`, or `openai` |
| Affinity | The choice of upstream from the session id (`cache_affine`), so a session keeps one account and its prompt cache |
| Failover | Resending a request to the route's next upstream after a refusal that proves it was not processed, before any output |
| Alias | A model name clients send that config maps to a real model, per token subject |
| Gateway token | What a caller presents to Wye (a JWT from Wye or an identity provider); Wye replaces it with the provider key |
| Hold | Reading the start of a request body until its `model` is known, for permissions, output-token ceilings, aliases, and provider model ids |
| Usage record | One JSON line per proxied request: tokens, cost, session, request class, outcome; also an OTLP log record and span |

## Conventions

- **Proposal / illustrative:** not yet implemented; shape may change.
- **Research:** idea to investigate, not a product promise.
- Do not claim performance improvements without published, reproducible benchmarks.
- Support is declared per **endpoint × provider × feature**. Do not imply more.
