# Wye

[![CI](https://github.com/askrid/wye/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/askrid/wye/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Rust 1.98+](https://img.shields.io/badge/rust-1.98%2B-orange)](rust-toolchain.toml)

Wye is a small, fast gateway between coding agents (Claude Code, Codex) or your
own apps and the model providers. Put it in front of your clients and it records
what every request used and cost, spreads traffic across your API keys and
providers with failover, and decides who may use which model. Each provider's
native API goes through byte for byte, so clients keep the official SDK and
nothing gets translated. One TOML file in Git holds every decision; Wye itself
is one Rust binary with no database, and in lab benchmarks adds about a quarter
of a millisecond to a request.

> **Status:** working prototype, `0.x`. It is tested and benchmarked on one
> machine and has been used with Claude Code through a claude.ai subscription.
> See [Status](#status) for what is and is not checked against the real
> providers.

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

```bash
cargo install --git https://github.com/askrid/wye wye --locked   # Rust 1.98+

wye init              # writes ~/.wye/wye.toml; needs no key
wye service install   # shows what it sets up and asks; runs the gateway in the background
wye claude            # Claude Code through the gateway, signed in as usual
wye report            # what it recorded
```

Your claude.ai login or API key goes through, on this machine only, and the
gateway holds no secret. Prebuilt archives for Linux and macOS are on the
[releases page](https://github.com/askrid/wye/releases).
[docs/guide.md](docs/guide.md) covers the other setups: a team behind HTTPS,
apps on the official SDKs, upgrades, and troubleshooting.

## What it does

- **Works with the clients you have.** Claude Code, Codex, and apps on the
  official Anthropic and OpenAI SDKs connect by base URL; `wye claude` and
  `wye codex` set it for you. Anthropic Messages, OpenAI Responses, and Chat
  Completions pass through as sent, and streams event by event, never buffered.
  ([running it](docs/guide.md#running-it), [apps](docs/guide.md#using-it-from-apps))
- **Shows usage and cost.** One record per request: tokens, cost, model,
  session, subagent, request class, tool calls and their time. `wye report`
  summarizes them with no backend; OTLP sends logs, metrics, and one trace per
  Claude Code session to yours. Prompt-cache misses are sorted into expected and
  unexplained. ([reading the report](docs/guide.md#reading-wye-report),
  [telemetry](docs/operations.md#telemetry), [prices](docs/policy.md#pricing-metadata))
- **Spreads load across API keys and providers.** Several Anthropic keys,
  Claude Platform on AWS, and Bedrock behind one address (several OpenAI keys
  too), shared by weight. Each session stays on one key, so its prompt cache
  stays warm, and a request refused with `429` or `529` moves to the next key
  before any output. Bedrock and Claude Platform on AWS are not yet checked
  against the live services.
  ([several keys or providers](docs/guide.md#several-keys-or-providers))
- **Gives a team its own access.** Gateway tokens signed by Wye or by your
  identity provider (GitHub Actions, or your SSO through Claude Code's
  `apiKeyHelper`), each with the models and output-token ceiling it may use,
  checked before the provider is called. Callers never see the provider key,
  and with `[discovery]` Claude Code's model picker lists only the models a
  token may use.
  ([teams](docs/guide.md#using-it-for-a-team), [tokens](docs/policy.md#gateway-tokens))
- **Changes models in one place.** Aliases map a stable name to a model per team
  or token, and upgrade checks report which requests a newer model would reject
  before you move. Requests are never changed to fit.
  ([aliases and upgrades](docs/guide.md#model-aliases-and-upgrades))
- **Checks decisions before they ship.** `wye explain` shows what the gateway
  would do with a request, offline; `wye policy-test` and `wye policy-diff` check
  policy in CI and fail when a change widens access; `wye doctor` checks keys and
  endpoints without billing anything. ([commands](docs/operations.md#commands))
- **Runs where you need it.** `wye service` installs a macOS LaunchAgent or a
  systemd user unit; for a team, any number of identical replicas behind a load
  balancer, with no coordination between them, health checks, and graceful
  shutdown. ([deployment](docs/operations.md#deployment-modes))

## Who it's for

- **One developer.** Tokens per session, subagent, and tool, prompt-cache
  misses flagged, and cost once the config has prices, all from `wye report`
  with no backend. Your login or key stays yours. Claude Code reports its own
  totals; Wye shows where they went.
- **A team.** A token per developer and CI job, provider keys only in the
  deployment, policy reviewed in Git and tested in CI, and usage per developer
  and session in your OpenTelemetry backend. If you run only Claude Code and
  sign in with SSO, Anthropic's own self-hosted gateway for Claude Code may fit
  better: it follows every Claude Code release.
- **Apps on the official SDKs.** Model names, keys, failover, and cost move out
  of application code, and a model upgrade is a config change once the upgrade
  check shows nothing breaks. Wye does not protect a public endpoint from abuse
  by itself: per-user sign-in and quotas stay in your backend.

## Speed

Wye is meant to never be the slow part. Every commit is benchmarked against
its parent, in alternating rounds on the same machine (`scripts/check.sh`),
and a change that makes Wye slower or heavier beyond the gate's tolerances is
not committed. The numbers below are lab numbers from one laptop (Intel
i9-9880H, macOS) against a local mock provider, not production measurements.

| What Wye adds | Plain HTTP/1.1 | TLS + HTTP/2 |
|---|---|---|
| Latency per request (p50) | 0.28 ms | 0.22 ms |
| CPU per request | 120 µs | 161 µs |
| CPU per streamed token event | 18.5 µs | |
| Heap allocations per streamed event | 0 | |
| 2,000 concurrent token-paced streams, delay per event (p50 / p99) | | 1.4 / 4.0 ms |
| Memory per open stream at that load | | 9.3 KB |

A heavy Claude Code user, about five streams at once, costs Wye about 1% of a
core and under 100 KB of memory ([sizing](docs/operations.md#sizing-estimate),
an estimate). The delay per event does not add up over a stream, so Wye does
not lower tokens per second.

Other gateways ran on the same laptop, each at its best configuration and in
the same run as Wye. Each cell is the other gateway's figure divided by Wye's
(TLS; streamed events with 2,000 token-paced streams):

| Gateway | Added latency (p50) | CPU per request | CPU per streamed event | Memory after load |
|---|---|---|---|---|
| [agentgateway](https://github.com/agentgateway/agentgateway) 1.7 (Rust) | 7× | 3.1× | 1.1× | 2.9× |
| [Bifrost](https://github.com/maximhq/bifrost) 2.3 (Go) | 11× | 6.6× | 4.4× | 21× |
| [LiteLLM](https://github.com/BerriAI/litellm) 1.105 (Python) | 170× | 120× | 8.9× | 320× (at idle) |
| [KrakenD](https://github.com/krakend/krakend-ce) 3.0 CE (Go) | 1.4× | 1.6× | cannot stream | 5.7× |

agentgateway beats Wye on paced-stream latency (1.0 vs 1.2 ms p50 in that run)
and on memory beside stalled clients, and KrakenD on bulk transfer and TLS
uploads.
[docs/benchmarks.md](docs/benchmarks.md) has the method, the full tables, the
configurations used, and where Wye loses.

## Why

A provider integration keeps needing work after it ships. On Anthropic's API,
in one year, `thinking.budget_tokens`, sampling parameters, assistant prefill,
forced `tool_choice`, and `output_format` all broke on newer models. Keys,
accounts, and rate limits change, and every service grows its own retries,
logging, and cost tracking. Many gateways take that work on by translating
every request into one common format, which then lags each new provider field.

Wye takes the opposite approach:

- It does not translate. Anthropic Messages, OpenAI Responses, and Chat
  Completions are forwarded as they are, so new provider features on the
  endpoints Wye serves work through it on the day they ship. The only body
  rewrite is the `model` field (an alias, or Bedrock's `anthropic.` model ids),
  spliced in place.
- It reads coding-agent traffic. Claude Code's session, subagent, request
  class, compaction, and tool timing headers become usage records, one trace
  per session, and an explanation of prompt-cache misses. Codex's session and
  subagent headers map onto the same fields.
- It keeps no state between requests. Limits are per request; rate limits and
  budgets stay with the provider, which already keeps that ledger.

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
goes to the other before any output; two keys of one provider work the same way
([how](docs/guide.md#several-keys-or-providers)). Secrets come from environment
variables or files, never from the config, and unknown fields stop startup.
[examples/wye.toml](examples/wye.toml) is the reference: every option with its
default and limits, kept valid by a test.

## What it looks like

Each request produces one JSON line on stdout (and, with `[telemetry]`, an
OTLP log record, a span, and metrics):

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

Costs are exact decimal strings, computed in integers. A model with no price is
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

Not checked against the real services yet:
- Bedrock and Claude Platform on AWS were built from the providers'
  documentation and tested against mocks. Bedrock's API key header in
  particular needs one live request to confirm.
- Codex support was built from Codex's source and tests, not from a real
  session.
- Claude Code runs through Wye on a claude.ai subscription (since 2026-10-30:
  streaming, tool use, prompt caching, extended thinking, token counting,
  background sessions); not yet with an API key.

Not built yet: a published container image, Vertex AI, AWS SigV4 signing,
per-request cost ceilings, budgets held by the provider, and reloading
short-lived provider keys without a restart.
[docs/roadmap.md](docs/roadmap.md) tracks all of it.

## What Wye will not do

- Global rate limits, spending budgets, or quota counters. They need shared
  state; providers already enforce them per key or workspace.
- Circuit breakers, cooldowns, or balancing by measured latency: they remember
  across requests. Failover handles a refusal within the request.
- Translate between provider APIs, rewrite prompts, swap in another model when
  one fails, or cache responses.
- Filter content (guardrails, redaction) or store prompts.
- Run agents or tools.
- Grow a dashboard or an admin API. Config changes go through Git and a
  restart or rolling deploy.

## Development

Tests use local mock upstreams and never call a paid API, and every commit
passes a benchmark gate against its parent:
[docs/development.md](docs/development.md).

## Documentation

[docs/README.md](docs/README.md) says which document answers what.
[examples/wye.toml](examples/wye.toml) lists every configuration option.

## License

[MIT](LICENSE).
