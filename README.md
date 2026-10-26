# Wye

A stateless gateway between AI agents (Claude Code, Codex) or AI-native apps and the model providers, in Rust. Clients keep the provider's official SDK and native API; model choice, providers and accounts, credentials, limits, failover, model upgrades, telemetry, and cost live in one Git-managed TOML file. One binary, no database, no control plane: run as many identical replicas as you like behind a load balancer.

- **Native passthrough.** Anthropic Messages, OpenAI Responses and Chat Completions, forwarded byte for byte: no translation, new provider features work on day one.
- **Agent-aware.** Sessions, subagents, request classes, compactions, tool time, cache misses, and cost per request, as usage records, `wye report`, and OpenTelemetry.
- **Policy as code.** Signed gateway credentials, model permissions, output-token ceilings, model aliases with staged rollout, and upgrade checks; `wye explain` and `wye policy-test` show every decision offline, and `wye policy-diff` fails CI when a config change widens access.
- **Several accounts and providers.** Each session stays on one account (its prompt cache stays warm), failing over before any output on a rate limit or overload.
- **Fast, and gated on it.** Every commit runs a benchmark gate: added latency, CPU per request and per event, memory per stream, allocations ([results](docs/research.md)).

## Quick start

From source (macOS or Linux, Rust from `rust-toolchain.toml`):

```bash
cargo install --path crates/wye --locked

# Claude Code through a claude.ai subscription (loopback only):
wye serve --config examples/subscription.toml 2>>usage.jsonl &
wye claude

# Or with an Anthropic API key:
ANTHROPIC_API_KEY=... wye serve --config examples/wye.toml 2>>usage.jsonl &
wye doctor --config examples/wye.toml   # checks keys, files, and endpoints
wye claude

wye report usage.jsonl                  # sessions, request classes, cache misses, cost
```

Without the repository's examples, `wye init` writes a minimal config (`--subscription`, `--openai-key-env OPENAI_API_KEY`) and prints the next commands.

SDK apps point their client's base URL at the gateway (`http://127.0.0.1:8080` for Anthropic, `http://127.0.0.1:8080/v1` for OpenAI). Codex: add `[routes.openai]` to the config and run `wye codex`.

## Documentation

[docs/README.md](docs/README.md) is the index: product scope and status, architecture, policy, operations (CLI, deployment, telemetry), roadmap, research and benchmarks, and the development workflow.

Status: a working prototype, built and benchmarked on one machine; not yet packaged or released.
