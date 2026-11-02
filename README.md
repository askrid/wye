# Wye

[![CI](https://github.com/askrid/wye/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/askrid/wye/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Rust 1.99+](https://img.shields.io/badge/rust-1.99%2B-orange)](rust-toolchain.toml)

**Run your agents like infrastructure.** Keys, models, failover, limits, and
cost for Claude Code, Codex, and your apps live in one TOML file in Git, and
every request passes through as it was sent.

Wye is a stateless gateway for coding agents and LLM applications. It sits
between clients and model providers, records the usage and cost of every
request, distributes traffic across API keys and providers with failover, and
enforces which caller may use which model. Clients keep their official SDKs:
Wye relays each provider's native API without translation. It ships as a
single Rust binary with no database and, in lab benchmarks, adds about 0.25 ms
to a request.

> **Status:** prerelease (`0.1.0-rc.1`). Tested and benchmarked on one
> machine, and in use with Claude Code on a claude.ai subscription. See
> [Status](#status) for what is not yet verified against live providers.

```console
$ wye report
3 requests in 1 session (0 other lines skipped); outcomes: completed 3

By request class
                                      cache  cache
  class           reqs  input output   read  write    hit      cost  time tools
  all                3     36    114   149K   4.6K  97.0%   $0.0553   14s  1.8s
  main               3     36    114   149K   4.6K  97.0%   $0.0553   14s  1.8s
...
```

## Quick start

```bash
cargo install --git https://github.com/askrid/wye wye --locked   # Rust 1.99+

wye init              # writes ~/.wye/wye.toml; needs no key
wye service install   # shows what it sets up and asks; runs the gateway in the background
wye claude            # Claude Code through the gateway, signed in as usual
wye report            # what it recorded
```

This default setup forwards the caller's own claude.ai login or API key, listens
on this machine only, and holds no secret. Prebuilt archives for Linux and
macOS are on the [releases page](https://github.com/askrid/wye/releases). The
[guide](docs/guide.md) covers the other setups: a team gateway behind HTTPS,
applications on the official SDKs, upgrades, and troubleshooting.

## What it does

- **Shows usage and cost.** One record per request with its tokens, cost, and
  model, plus the session, subagent, request class, and tool calls for Claude
  Code and Codex. `wye report` summarizes the log without a backend; OTLP
  exports logs, metrics, and one trace per Claude Code session to yours.
  Prompt-cache misses are classified as expected or unexplained.
  ([reading the report](docs/guide.md#reading-wye-report),
  [telemetry](docs/operations.md#telemetry), [prices](docs/policy.md#pricing-metadata))
- **Gives a team scoped access.** Gateway tokens replace the provider key,
  signed by Wye or by your identity provider (GitHub Actions, or SSO through
  Claude Code's `apiKeyHelper`). Each token carries the models and output-token
  ceiling it may use, enforced before the provider is called; with
  `[discovery]`, Claude Code's model picker lists only those models.
  ([teams](docs/guide.md#using-it-for-a-team), [tokens](docs/policy.md#gateway-tokens))
- **Spreads load across keys and providers.** Several Anthropic keys, Claude
  Platform on AWS, and Bedrock behind one address (several OpenAI keys too),
  shared by weight. Each session stays on one key so its prompt cache stays
  warm, and a request refused with `429` or `529` moves to the next key before
  any output. Bedrock and Claude Platform on AWS are not yet verified against
  the live services.
  ([several keys or providers](docs/guide.md#several-keys-or-providers))
- **Changes models in one place.** Aliases map a stable name to a model per
  team or token, and upgrade checks report which requests a newer model would
  reject before the switch. Requests are never altered to fit.
  ([aliases and upgrades](docs/guide.md#model-aliases-and-upgrades))
- **Checks decisions before they ship.** `wye explain` shows what the gateway
  would do with a request, offline. `wye policy-test` and `wye policy-diff`
  check policy in CI and fail when a change widens access. `wye doctor` checks
  keys and endpoints without billing anything.
  ([commands](docs/operations.md#commands))
- **Works with existing clients.** Claude Code, Codex, and applications on the
  official Anthropic and OpenAI SDKs connect by base URL; `wye claude` and
  `wye codex` set it for you. Anthropic Messages, OpenAI Responses, and Chat
  Completions pass through as sent, apart from a model id that config maps,
  and streams are relayed as the provider sends them, never buffered.
  ([running it](docs/guide.md#running-it), [apps](docs/guide.md#using-it-from-apps))
- **Runs where you need it.** `wye service` installs a macOS LaunchAgent
  or a systemd user unit. A team runs any number of identical replicas behind a
  load balancer, with no coordination between them, health checks, and
  graceful shutdown. ([deployment](docs/operations.md#deployment-modes))

## Who it's for

- **Individual developers.** See where tokens and money go, per session,
  subagent, and tool, with prompt-cache misses flagged and cost once the config
  has prices, from `wye report` alone. Next (planned): which tokens were waste,
  and what each fix would save. The caller's own login or key is used; the
  gateway holds no secret.
- **Teams.** A token per developer and CI job, provider keys only in the
  deployment, policy reviewed in Git and tested in CI, and usage per developer
  and session in your OpenTelemetry backend. A team that runs only Claude Code
  with SSO may prefer Anthropic's own self-hosted gateway for Claude Code, which
  follows every Claude Code release.
- **Applications on the official SDKs.** Model names, keys, failover, and cost
  move out of application code, and a model upgrade becomes a config change
  once the upgrade check reports nothing that would break. Wye alone does not
  protect a public endpoint from abuse: per-user authentication and quotas stay
  in your backend.

## How it compares

What teams usually expect from an LLM gateway, and Wye's answer. The
differences follow from two choices: each client keeps its provider's own API,
and Wye keeps no state between requests.

| Teams usually want | Wye |
|---|---|
| Spend by team, user, and app | Yes, down to the agent session, subagent, and tool, with prompt-cache misses explained |
| Scoped keys instead of the provider key | Yes: a signed token per developer, CI job, or app, from Wye or your identity provider |
| Spend caps and rate limits | Per-request output ceilings today, and the provider's own rate limits. Spend caps per team come after the waste report, enforced by the provider on each team's own key (planned); Wye keeps no counters |
| Failover and load across keys | Yes, before any output, each session kept on one key so its prompt cache stays warm |
| Logs, metrics, and traces | Yes, in your OpenTelemetry backend, one trace per Claude Code session |
| Switching models centrally | Yes, and upgrade checks report what a newer model would reject before the switch |
| Policy you can review and test | Yes: one TOML file in Git, checked in CI by `wye policy-test` and `wye policy-diff` |
| One API for every provider | No: each client keeps its provider's API. For many providers behind one API, LiteLLM or OpenRouter fit better |
| Guardrails, redaction, response caching | No ([why](#what-wye-will-not-do)) |
| Little to operate | One binary, no database ([performance](#performance)) |

Why native APIs: a provider integration keeps needing work after it ships. On
Anthropic's API, within one year, `thinking.budget_tokens`, sampling
parameters, assistant prefill, forced `tool_choice`, and `output_format` all
broke on newer models. A gateway that translates every request into one format
lags each such change. Wye forwards requests as sent, so new provider features
on the endpoints it serves work on the day they ship. The only body field it
changes is `model` (an alias, or Bedrock's `anthropic.` ids), spliced in place,
and the response names the model that ran.

## Performance

Every commit that changes code is benchmarked against its parent in
alternating rounds on the same machine (`scripts/check.sh`), and a change
that makes Wye slower or heavier beyond the gate's tolerances is not
committed. The figures below are
lab results from one laptop (Intel i9-9880H, macOS) against a local mock
provider, not production measurements.

| What Wye adds | Plain HTTP/1.1 | TLS + HTTP/2 |
|---|---|---|
| Latency per request (p50) | 0.28 ms | 0.22 ms |
| CPU per request | 120 µs | 161 µs |
| CPU per streamed token event | 18.5 µs | |
| Heap allocations per streamed event | 0 | |
| 2,000 concurrent token-paced streams, delay per event (p50 / p99) | | 1.4 / 4.0 ms |
| Memory per open stream at that load | | 9.3 KB |

A heavy Claude Code user, about five concurrent streams, costs about 1% of a
core and 250 KB of memory ([sizing](docs/operations.md#sizing-estimate), an
estimate). The added delay per event does not accumulate over a stream, so
tokens per second are unchanged.

Other gateways ran on the same laptop, each at its best configuration and in
the same run as Wye. Each cell is the other gateway's figure divided by Wye's
(TLS; streamed events measured with 2,000 token-paced streams):

| Gateway | Added latency (p50) | CPU per request | CPU per streamed event | Memory after load |
|---|---|---|---|---|
| [agentgateway](https://github.com/agentgateway/agentgateway) 1.7 (Rust) | 7× | 3.1× | 1.1× | 2.9× |
| [Bifrost](https://github.com/maximhq/bifrost) 2.3 (Go) | 11× | 6.6× | 4.4× | 21× |
| [LiteLLM](https://github.com/BerriAI/litellm) 1.105 (Python) | 170× | 120× | 8.9× | 320× (at idle) |
| [KrakenD](https://github.com/krakend/krakend-ce) 3.0 CE (Go) | 1.4× | 1.6× | cannot stream | 5.7× |

Where others lead: agentgateway on paced-stream latency (1.0 vs 1.2 ms p50 in
that run), event latency during uploads, cold connections, and memory beside
stalled clients; KrakenD on bulk transfer and TLS uploads.
[docs/benchmarks.md](docs/benchmarks.md) has the method, the full tables, and
each configuration.

## Configuration

A route with two accounts, failover, and a price table:

```toml
[server]
listen = "127.0.0.1:8080"

[routes.claude]
failover = ["429", "529", "connect"]

[[routes.claude.upstreams]]
id = "anthropic"
weight = 2         # twice the sessions of the other
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"

[[routes.claude.upstreams]]
id = "bedrock-us"
provider = "bedrock"
base_url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"
api_key_env = "BEDROCK_API_KEY"

[pricing]
version = "anthropic-2026-11-01"
[[pricing.models]]
models = ["claude-opus-5-5"]
input = "4.00"     # USD per million tokens
output = "20.00"
cache_read = "0.20"
cache_write_5m = "5.00"
cache_write_1h = "8.00"
```

Each session stays on one account, and a request refused with `429` or `529`
moves to the other before any output; two keys of one provider work the same
way ([details](docs/guide.md#several-keys-or-providers)). Secrets come from
environment variables or files, never from the config, and an unknown field
stops startup. [examples/wye.toml](examples/wye.toml) is the reference: every
option with its default and limits, kept valid by a test.

## Records and decisions

Each request produces one JSON line on stdout and, with `[telemetry]`, an OTLP
log record, a span, and metrics:

```json
{
  "event": "request",
  "unix_ms": 1793178637975,
  "path": "/v1/messages",
  "status": 200,
  "outcome": "completed",
  "duration_ms": 4100.42,
  "headers_ms": 651.8,
  "upstream": "bedrock-us",
  "session_id": "7f2c4a10-8e1b-4d3a-9c55-2b6e0d1f9a33",
  "request_class": "main",
  "model": "claude-opus-5-5",
  "usage_status": "complete",
  "input_tokens": 12,
  "output_tokens": 38,
  "cache_creation_input_tokens": 1530,
  "cache_read_input_tokens": 48210,
  "cost_usd": "0.0181",
  "cost_status": "complete",
  "pricing_version": "anthropic-2026-11-01",
  "tool_calls": { "Bash": 1 }
}
```

Costs are exact decimal strings computed in integers. A model with no price is
recorded as `unknown`, never as $0.

`wye explain` shows what the server would do with a request, offline, by
calling the same functions in the same order:

```console
$ wye explain -c wye.toml --session session-a request.json
config: wye.toml
endpoint: POST /v1/messages
token: not required (no [auth])
body: 88 bytes (limit 32 MiB)
model: claude-opus-5-5; allowed by config: any; by token: any
route: upstream bedrock-us of 2 (cache_affine by session id)
failover: on 429, 529, connect to anthropic in that order, for bodies up to 8 MiB
upstream: POST https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages
provider key: x-api-key from $BEDROCK_API_KEY
model id: the body names anthropic.claude-opus-5-5 upstream, the provider's id for claude-opus-5-5
price: claude-opus-5-5 at bedrock-us: input $4.00, output $20.00, cache read $0.20, cache write 5m $5.00, cache write 1h $8.00 per million tokens (pricing version anthropic-2026-11-01)
decision: forward
```

## Status

Built and covered by tests: everything listed under [What it does](#what-it-does).

Not yet verified against the live services:
- Bedrock and Claude Platform on AWS were built from the providers'
  documentation and tested against mocks. Bedrock's API key header in
  particular needs one live request to confirm.
- Codex support was built from Codex's source and tests, not from a real
  session.
- Claude Code runs through Wye on a claude.ai subscription (since 2026-10-30:
  streaming, tool use, prompt caching, extended thinking, token counting,
  background sessions), not yet with an API key.

Next (planned): a report of which tokens were waste and what each fix saves,
and a smaller model for small background requests; then spend caps per team,
enforced by the provider on each team's own key.

Not built yet: a published container image, Vertex AI, AWS SigV4 signing,
per-request cost ceilings, and reloading short-lived provider keys without a
restart. [docs/roadmap.md](docs/roadmap.md) tracks all of it.

## What Wye will not do

- Global rate limits, spending budgets, or quota counters. They need shared
  state; providers already enforce them per key or workspace.
- Circuit breakers, cooldowns, or balancing by measured latency: they remember
  across requests. Failover handles a refusal within the request.
- Translate between provider APIs, rewrite prompts, swap in another model when
  one fails, or cache responses.
- Filter content (guardrails, redaction) or store prompts.
- Terminate TLS: a load balancer, an ingress, or Caddy in front does it, and
  renews the certificate.
- Run agents or tools.
- Grow a dashboard or an admin API. Config changes go through Git and a
  restart or rolling deploy.

## Development

Tests run against local mock upstreams and never call a paid API, and every
commit that changes code passes a benchmark gate against its parent. See
[docs/development.md](docs/development.md).

## Documentation

[docs/README.md](docs/README.md) maps each question to the document that
answers it, and [examples/wye.toml](examples/wye.toml) lists every
configuration option.

## License

[MIT](LICENSE).
