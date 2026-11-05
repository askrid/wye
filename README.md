<div align="center">

<h1><img src="site/assets/icon-fork.svg" alt="" width="44" align="absmiddle"> Wye</h1>

**LLM gateway without bloat.**

[![CI](https://img.shields.io/github/actions/workflow/status/askrid/wye/ci.yml?branch=main&style=flat-square&label=ci)](https://github.com/askrid/wye/actions/workflows/ci.yml) [![Release](https://img.shields.io/github/v/release/askrid/wye?include_prereleases&style=flat-square&label=release&color=555)](https://github.com/askrid/wye/releases) [![License](https://img.shields.io/github/license/askrid/wye?style=flat-square&color=555)](LICENSE)

[Quick start](#quick-start) · [Guide](docs/guide.md) · [Configuration](examples/wye.toml) · [Benchmarks](docs/benchmarks.md) · [Website](https://askrid.github.io/wye/)

</div>

- **Fast.** Adds under a millisecond per request ([p50, lab](#performance)).
- **Stateless.** Scale by running more replicas.
- **Fails over.** A rejected request can be resent to another upstream before any output.
- **One binary, one TOML file.** No database, no dashboard.
- **Untranslated.** Native APIs pass through, so new features work the day they ship.

## Performance

Lab results from one laptop (Intel i9-9880H, macOS) against a local mock provider. Not production. Each gateway ran in the same run as Wye, at its best configuration, and each cell is its number divided by Wye's (TLS; stream events with 2,000 token-paced streams). The runs are from 2026-10-15 (KrakenD, LiteLLM) to 2026-11-01 (agentgateway), so Wye's build differs between rows:

| Gateway | Added latency (p50) | CPU per request | CPU per streamed event | Memory after load |
|---|---|---|---|---|
| [agentgateway](https://github.com/agentgateway/agentgateway) 1.7 (Rust) | 7× | 3.1× | 1.1× | 2.9× |
| [Bifrost](https://github.com/maximhq/bifrost) 2.3 (Go) | 11× | 6.6× | 4.4× | 21× |
| [LiteLLM](https://github.com/BerriAI/litellm) 1.105 (Python) | 170× | 120× | 8.9× | 320× (at idle) |
| [KrakenD](https://github.com/krakend/krakend-ce) 3.0 CE (Go) | 1.4× | 1.6× | cannot stream | 5.7× |

Where others lead: agentgateway on paced-stream latency (1.0 vs 1.2 ms p50 in that run), event latency during uploads, cold connections, and memory next to stalled clients. KrakenD on bulk transfer and TLS uploads. Method, full tables, and configs: [docs/benchmarks.md](docs/benchmarks.md).

What Wye itself adds:

| What Wye adds | Plain HTTP/1.1 | TLS + HTTP/2 |
|---|---|---|
| Latency per request (p50) | 0.28 ms | 0.22 ms |
| CPU per request | 120 µs | 161 µs |
| Heap allocations per streamed event | 0 | |
| 2,000 concurrent token-paced streams, delay per event (p50 / p99) | | 1.4 / 4.0 ms |
| CPU per event and memory per open stream at that load | | 37 µs, 9.3 KB |

A heavy Claude Code user (about five concurrent streams) takes about 1% of a core and 250 KB of memory ([sizing](docs/operations.md#sizing-estimate), an estimate). The per-event delay doesn't add up over a stream, so tokens per second don't change. On my laptop, 35 hours of Claude Code sessions (2,562 requests) took 31 s of CPU and 25 MB of memory ([in use](docs/benchmarks.md#in-use)).

Every commit that changes code is benchmarked against its parent, in alternating rounds on the same machine (`scripts/check.sh`). If it makes Wye slower or heavier beyond the commit gate's tolerances, it doesn't get in.

## Quick start

> **Status:** prerelease (`0.1.0-rc.1`), tested and benchmarked on one machine. [Status](#status) says what is not yet verified against live providers.

```bash
cargo install --git https://github.com/askrid/wye wye --locked   # Rust 1.99+

wye init              # writes ~/.wye/wye.toml; needs no key
wye service install   # shows what it sets up and asks; runs the gateway in the background
wye claude            # Claude Code through the gateway, signed in as usual
wye report            # what it recorded
```

Passthrough, the default setup, forwards the caller's own claude.ai login or API key, listens on this machine only, and holds no secret. Prebuilt archives for Linux and macOS are on the [releases page](https://github.com/askrid/wye/releases). The [guide](docs/guide.md) covers the other setups: a gateway that holds a provider key, a shared gateway behind HTTPS for a team or an app's services, upgrades, and troubleshooting.

After a short Claude Code session:

```console
$ wye report
3 requests in 1 session (0 other lines skipped); outcomes: completed 3

By request class
                                      cache  cache                   time  time
  class           reqs  input output   read  write    hit      cost model tools
  all                3     36    114   149K   4.6K  97.0%   $0.0553   14s  1.8s
  main               3     36    114   149K   4.6K  97.0%   $0.0553   14s  1.8s
...
```

## What it does

- **Shows usage and cost.** One JSON line per request: tokens, cost, model, and for Claude Code and Codex the session, subagent, request class, and tool calls. `wye report` reads the usage log, no backend needed. OTLP sends logs, metrics, and one trace per Claude Code session to yours. Every prompt-cache miss comes with its cause (`compaction`, `after_compaction`, or `unexplained`), and `wye report` counts session and subagent starts as expected. ([reading the report](docs/guide.md#reading-wye-report), [telemetry](docs/operations.md#usage-records-and-telemetry), [prices](docs/policy.md#price-table))
- **Gives a team scoped access.** Gateway tokens instead of the provider key, signed by Wye or by your identity provider (GitHub Actions, or SSO through Claude Code's `apiKeyHelper`). A token names its models and its output-token ceiling; anything else is refused before the provider gets the whole request. With `[discovery]` and `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`, Claude Code's model picker shows only those models. ([teams](docs/guide.md#using-it-for-a-team), [tokens](docs/policy.md#gateway-tokens))
- **Spreads load across keys and providers.** Several Anthropic keys, Claude Platform on AWS, and Bedrock behind one address (several OpenAI keys too), split by weight, with failover between them. Each key or account is an [upstream](docs/glossary.md#requests-and-where-they-go). ([several keys or providers](docs/guide.md#several-keys-or-providers))
- **Changes models in one place.** Aliases map a name to a model, per gateway-token subject. Upgrade checks tell you which Messages requests a newer model would reject, before you switch. Wye never rewrites a request to make it fit. ([aliases and upgrades](docs/guide.md#model-aliases-and-upgrades))
- **Checks decisions before they ship.** `wye explain` shows what the gateway would do with a request, offline. `wye policy-test` and `wye policy-diff` run in CI and fail when a change widens access. `wye doctor` checks secrets, files, and upstreams, and bills nothing. ([commands](docs/operations.md#commands))
- **Works with existing clients.** Point Claude Code, Codex, or any app on the official Anthropic or OpenAI SDK at it by base URL; `wye claude` and `wye codex` do it for you. Messages, Responses, and Chat Completions go through untranslated, except a model id your config maps. Streams are relayed as the provider sends them, never buffered. ([running it](docs/guide.md#running-it), [apps](docs/guide.md#using-it-from-apps))
- **Runs as one process, or many.** `wye service` installs a macOS LaunchAgent or a systemd user unit. For more traffic, run more replicas behind any load balancer. They share no state and never talk to each other, and every replica sends a session to the same upstream (hashed from its session id), so prompt caches stay warm without sticky sessions. Each has health checks and a graceful shutdown. ([deployment](docs/operations.md#deployment))

## Who it's for

- **Individual developers.** See where tokens and money go, per session, subagent, and tool, with prompt-cache misses flagged and cost once the config has prices, from `wye report` alone, with no secret in the gateway.
- **Teams.** A gateway token per developer and CI job, provider keys only in the deployment, policy reviewed in Git and tested in CI, and usage per developer and session in your OpenTelemetry backend. A team that runs only Claude Code with SSO may prefer Anthropic's own self-hosted gateway for Claude Code, which follows every Claude Code release.
- **Applications on the official SDKs.** Model names, keys, failover, and cost move out of application code, and a model upgrade becomes a config change once the upgrade check reports nothing that would break (it reads Anthropic Messages requests). Wye alone does not protect a public endpoint from abuse: per-user authentication and quotas stay in your backend.

## How it compares

What teams usually expect from an LLM gateway, and what Wye does. Two choices explain most answers: every client keeps its provider's own API, and Wye keeps no state between requests.

| Teams usually want | Wye |
|---|---|
| Spend by team, user, and app | Yes. Down to the agent session, subagent, and tool, with prompt-cache misses explained |
| Scoped keys instead of the provider key | Yes. A signed gateway token per developer, CI job, or app, from Wye or your identity provider |
| Spend caps and rate limits | Partly. Per-request output-token ceilings, and the provider's own limits; Wye keeps no counters. Provider-held budgets per team are [planned](docs/roadmap.md#m2-cost-policy-for-claude) |
| Failover and load across keys | Yes. Shared by weight, with failover before any output ([configuration](#configuration)) |
| Logs, metrics, and traces | Yes. In your OpenTelemetry backend, one trace per Claude Code session |
| Switching models centrally | Yes. Aliases, and upgrade checks report which Messages requests a newer model would reject before the switch |
| Policy you can review and test | Yes. One TOML file in Git, checked in CI by `wye policy-test` and `wye policy-diff` |
| One API for every provider | No. Each client keeps its provider's API. For many providers behind one API, LiteLLM or OpenRouter fit better |
| Guardrails, redaction, response caching | No. [What Wye will not do](#what-wye-will-not-do) says why |
| Scale out by adding instances | Yes. Replicas share no state, so you add them behind any load balancer. The provider keys' rate limits are the ceiling; more keys raise it |
| Little to operate | Yes. One binary, no database ([performance](#performance)) |

Why native APIs: provider APIs keep changing. Within one year on Anthropic's API, `thinking.budget_tokens`, sampling parameters, assistant prefill, forced `tool_choice`, and `output_format` all broke on newer models. A gateway that translates everything into one format is always catching up. Wye passes each native API through, so new features on the endpoints it serves work the day they ship ([what it changes](docs/architecture.md#forwarding-contract)).

## Configuration

A route with two upstreams, failover, and a price table:

```toml
[server]
listen = "127.0.0.1:8080"

[routes.claude]
failover = ["429", "529", "connect"]

[[routes.claude.upstreams]]
id = "anthropic"
weight = 2         # twice the sessions of the other
base_url = "https://api.anthropic.com"
provider_key_env = "ANTHROPIC_API_KEY"

[[routes.claude.upstreams]]
id = "bedrock-us"
provider = "bedrock"
base_url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"
provider_key_env = "BEDROCK_API_KEY"

[pricing]
version = "anthropic-2026-11-01"
[[pricing.prices]]
models = ["claude-opus-5-5"]
input = "4.00"     # USD per million tokens
output = "20.00"
cache_read = "0.20"
cache_write_5m = "5.00"
cache_write_1h = "8.00"
```

Each session stays on one upstream. A request rejected with `429` or `529` moves to the other before any output; two keys of one provider work the same way ([details](docs/guide.md#several-keys-or-providers)). Secrets come from environment variables or files, never the config. An unknown field stops startup. [examples/wye.toml](examples/wye.toml) has every option with its default and limits, and a test keeps it valid.

## Records and decisions

Each request produces one JSON line on stdout and, with `[telemetry]`, an OTLP log record, a span, and metrics:

```json
{
  "event": "request",
  "unix_ms": 1793178637975,
  "path": "/v1/messages",
  "status": 200,
  "outcome": "completed",
  "duration_ms": 4100.42,
  "response_headers_ms": 651.8,
  "upstream": "bedrock-us",
  "subject": "alice",
  "session_id": "7f2c4a10-8e1b-4d3a-9c55-2b6e0d1f9a33",
  "request_class": "main",
  "requested_model": "claude-coding",
  "model": "claude-opus-5-5",
  "usage_status": "complete",
  "input_tokens": 12,
  "output_tokens": 38,
  "cache_creation_input_tokens": 1530,
  "cache_read_input_tokens": 48210,
  "cost_usd": "0.0181",
  "cost_status": "complete",
  "pricing_version": "anthropic-2026-11-01",
  "tool_calls": { "Bash": 1 },
  "failovers": [{ "upstream": "anthropic", "reason": "429" }]
}
```

Costs are exact decimal strings, computed in integers. A model with no price is recorded as [`unknown`](docs/glossary.md#usage-and-cost), never $0.

`wye explain` shows what the gateway would do with a request, offline, by calling the same functions in the same order:

```console
$ wye explain -c wye.toml --session session-a request.json
config: wye.toml
endpoint: POST /v1/messages
gateway token: not required (no [auth])
body: 88 bytes (limit 32 MiB)
model: claude-opus-5-5; allowed by config: any; by token: any
selection: upstream bedrock-us of 2 (cache_affine by session id)
failover: on 429, 529, connect to anthropic in that order, for bodies up to 8 MiB
upstream: POST https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages
provider key: x-api-key from $BEDROCK_API_KEY
model id: the body names anthropic.claude-opus-5-5 upstream, the provider's id for claude-opus-5-5
price: claude-opus-5-5 at bedrock-us: input $4.00, output $20.00, cache read $0.20, cache write 5m $5.00, cache write 1h $8.00 per million tokens (pricing version anthropic-2026-11-01)
decision: forward
```

## Status

Built and covered by tests: everything under [What it does](#what-it-does). I've run Claude Code through Wye on a claude.ai subscription since 2026-10-30.

Tested only against mocks so far: Claude Code with an API key, Codex, Bedrock, and Claude Platform on AWS. `wye service` hasn't run under systemd on a real Linux machine.

Next: a report of which tokens were wasted and what each fix would save, and a smaller model for small background requests. Then provider-held budgets per team. Not built: a published container image, Vertex AI, AWS SigV4. Details in [docs/roadmap.md](docs/roadmap.md#status).

## What Wye will not do

- Global rate limits, spending budgets, or quota counters. They need shared state; providers already enforce them per key or workspace.
- Circuit breakers, cooldowns, or balancing by measured latency: they remember across requests. Failover handles a rejection within the request.
- Translate between provider APIs, rewrite prompts, swap in another model when one fails, or cache responses.
- Filter content (guardrails, redaction) or store prompts.
- Terminate TLS: a load balancer, an ingress, or Caddy in front does it, and renews the certificate.
- Run agents or tools.
- Grow a dashboard or an admin API. Config changes go through Git and a restart or rolling deploy.

## Development

Tests run against local mock upstreams and never call a paid API. See [docs/development.md](docs/development.md).

## Documentation

[docs/README.md](docs/README.md) maps each question to the document that answers it, [docs/glossary.md](docs/glossary.md) defines the terms, and [examples/wye.toml](examples/wye.toml) lists every configuration option.

## License

[MIT](LICENSE).
