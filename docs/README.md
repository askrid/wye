# Wye docs

New here: the [README](../README.md#what-it-does) says what Wye does, how fast it is, and [how it compares](../README.md#how-it-compares); the [glossary](glossary.md) defines its terms; the [guide](guide.md) shows how to use each part. Each document owns one concern, and the others link to it instead of restating it ([conventions](development.md#documentation)).

## Use it

- [guide.md](guide.md): choosing a setup, running it, reading `wye report`, teams, apps, several keys or providers, costs, model aliases and upgrades, an OTel backend, troubleshooting, FAQ.
- [examples/wye.toml](../examples/wye.toml): every configuration field, with its default, limits, and meaning (parsed by a test with every section enabled).
- [CHANGELOG.md](../CHANGELOG.md): what each release contains and what it leaves unverified.

## Run it

- [operations.md](operations.md): distribution, CLI, deployment topologies, telemetry, health, shutdown, failure behavior.

## Trust it

- [benchmarks.md](benchmarks.md): Wye's overhead, the method, competitors, the gate's evidence.
- [policy.md](policy.md): what Wye authorizes and enforces, what it cannot enforce without shared state, security requirements.
- [architecture.md](architecture.md): how a request flows: components, runtime, forwarding, providers, streaming, configuration, invariants, open design decisions.

## Change it

- [development.md](development.md): repository layout, commit gate, CI, benchmark scenarios, releases, code rules, documentation conventions, commit messages, review rounds.
- [roadmap.md](roadmap.md): status, open work, exit criteria, open findings, release checklists.
- [research.md](research.md): facts to verify, related work, feature directions (the cost bound among them), visibility for one developer, where the tokens go, rejected approaches, performance techniques.
- [product.md](product.md): why Wye exists, its principles, its non-goals (linked), and where it stands among alternatives.

## Terms

[glossary.md](glossary.md) defines each term the docs, the CLI, and the code use, and the names retired for it.
