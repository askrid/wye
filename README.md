# Wye

[![CI](https://github.com/askrid/wye/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/askrid/wye/actions/workflows/ci.yml)
[![License: WTFPL](https://img.shields.io/badge/license-WTFPL-blue)](LICENSE)
[![Rust 1.98+](https://img.shields.io/badge/rust-1.98%2B-orange)](rust-toolchain.toml)

Wye is a small, stateless gateway that sits between coding agents (Claude Code,
Codex) or your own apps and the model providers. It speaks each provider's
native API and forwards requests byte for byte, so clients keep the official
SDK and nothing gets translated. What changes is where the decisions live:
which provider and account a request goes to, who may use which model,
output-token ceilings, failover, model upgrades, and prices all sit in one TOML
file you keep in Git.

It is one Rust binary with no database and no admin API. Every replica makes
the same decision for the same request, so you can run as many as you like
behind a load balancer.

> **Status:** working prototype, `0.x`. It is tested and benchmarked on one
> machine and has been used with Claude Code through a claude.ai subscription.
> See [Status](#status) for what is and is not checked against the real
> providers.

## Why

Teams that run Claude Code or Codex at scale end up wanting the same things:
spend per team and per session, the same model behind a stable name while it
gets upgraded, more than one account or provider, and limits that apply before
a request reaches the provider. Most gateways get there by translating every
request into one common format, which breaks the moment a provider ships a new
field.

Wye takes the opposite approach:

- It does not translate. Anthropic Messages, OpenAI Responses, and Chat
  Completions are forwarded as they are, so new provider features on the
  endpoints Wye serves work through it on the day they ship. The only body rewrite is the `model` field (an
  alias, or Bedrock's `anthropic.` model ids), spliced in place.
- It reads coding-agent traffic. Claude Code's session, subagent, request
  class, compaction, and tool timing headers become usage records, one trace
  per session, and an explanation of prompt-cache misses. Codex's session and
  subagent headers map onto the same fields.
- It keeps no state between requests. Limits are per request; rate limits and
  budgets stay with the provider, which already keeps that ledger.

## Quick start

Build and install from source (needs Rust 1.98 or later):

```bash
cargo install --git https://github.com/askrid/wye wye --locked
```

Prebuilt Linux and macOS archives come with the first release, which is not
published yet.

### For yourself

```bash
wye init              # writes ~/.wye/wye.toml; needs no key
wye service install   # shows what it sets up and asks; runs the gateway in the background
wye claude            # Claude Code through the gateway, signed in as usual
wye report            # what it recorded
```

Your claude.ai login (Pro or Max) or API key goes through the gateway, on this
machine only, and the gateway holds no secret. As without Wye, an exported
`ANTHROPIC_API_KEY` takes over from your login and bills the API. `wye init
--api-key` has the gateway hold a key instead, and `wye serve >>
~/.wye/usage.jsonl` runs it in a terminal instead of as a service.

`wye claude` starts Claude Code with its base URL set to the gateway and its
gateway hints on (request class, prompt id, and tool times, for the usage
log), and `wye claude --bg "task"` starts a background session that goes
through it too. It does not touch your Claude Code settings.
`wye service install` shows the file it writes (a LaunchAgent on macOS, a
systemd user unit on Linux) and asks before changing anything;
`wye service uninstall` removes it
([docs/operations.md](docs/operations.md#running-as-a-service)).

### For a team

One gateway for every developer and app, configured from a repository. No image
is published yet, so build one from a clone of this repository:

```bash
docker build -t wye .                               # in a clone of Wye

wye init --team                                     # in your config repository: ./wye.toml,
                                                    # and a signing key in ~/.wye/issuer.key
wye token issue alice -c wye.toml --out alice.token # a token, valid for a week
docker run -p 8080:8080 -v ./wye.toml:/etc/wye/wye.toml:ro -e ANTHROPIC_API_KEY wye
```

Put it behind HTTPS ([docs/operations.md](docs/operations.md#deployment-modes)).
Each developer then points their agent at it:

```bash
export WYE_URL=https://wye.example.com WYE_TOKEN=<their token>
wye claude
```

Without installing Wye, Claude Code takes `ANTHROPIC_BASE_URL` and
`ANTHROPIC_AUTH_TOKEN`, or an `apiKeyHelper` that fetches short-lived tokens
from your identity provider
([docs/operations.md](docs/operations.md#token-renewal)). `wye codex` needs
`[routes.openai]` in the config (`wye init --team --openai`, and
`-e OPENAI_API_KEY` for the container).

### For apps

The official SDKs read their base URL and key from the environment, so an app
changes no code. Against a team's gateway, the key is a gateway token:

```bash
ANTHROPIC_BASE_URL=https://wye.example.com ANTHROPIC_API_KEY=<gateway token> python app.py
OPENAI_BASE_URL=https://wye.example.com/v1 OPENAI_API_KEY=<gateway token> node app.js
```

Against the gateway on your own machine, only the URL changes
(`ANTHROPIC_BASE_URL=http://127.0.0.1:8080`): the app's own key goes through.
With `wye init --api-key` the gateway holds the key, and the app's can be any
placeholder.

## Configuration

A route with two accounts, failover, and a price table:

```toml
[server]
listen = "127.0.0.1:8080"

[routes.claude]
failover = ["429", "529", "connect"]

[[routes.claude.upstreams]]
id = "anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"

[[routes.claude.upstreams]]
id = "bedrock-us"
provider = "bedrock"
base_url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"
api_key_env = "BEDROCK_API_KEY"

[pricing]
version = "anthropic-2026-10-06"
[[pricing.models]]
models = ["claude-opus-5-5"]
input = "4.00"     # USD per million tokens
output = "20.00"
cache_read = "0.40"
cache_write_5m = "5.00"
cache_write_1h = "8.00"
```

Each Claude Code session stays on one account, chosen by hashing its session
id, so its prompt cache stays warm and every replica picks the same account. If
that account answers `429` or `529`, or the connection fails before the request
body was delivered, Wye sends the request to the next account in the same turn,
never after the first response byte. This relies on `429` and `529` meaning the
provider did not process the request (not yet confirmed for every provider).

Secrets are read from environment variables or files and never appear in the
config. Unknown fields are rejected at startup.
[examples/wye.toml](examples/wye.toml) is the reference: every option with its
default and limits, kept valid by a test.

## What it looks like

Each request produces one JSON line on stdout (and, with `[telemetry]`, an
OTLP log record, a span, and metrics). This record is from a local test run:

```json
{
  "event": "request",
  "unix_ms": 1793178637975,
  "path": "/v1/messages",
  "status": 200,
  "outcome": "completed",
  "duration_ms": 3.07,
  "headers_ms": 2.65,
  "upstream": "bedrock-us",
  "session_id": "7f2c4a10-8e1b-4d3a-9c55-2b6e0d1f9a33",
  "request_class": "main",
  "model": "claude-opus-5-5",
  "usage_status": "complete",
  "input_tokens": 12,
  "output_tokens": 38,
  "cache_creation_input_tokens": 1530,
  "cache_read_input_tokens": 48210,
  "cost_usd": "0.027742",
  "cost_status": "complete",
  "pricing_version": "anthropic-2026-10-06",
  "tool_calls": { "Bash": 1 }
}
```

Costs are exact decimal strings, computed in integers. A model with no price is
recorded as `unknown`, never as $0.

`wye report` summarizes a log without any backend:

```console
$ wye report
3 requests in 1 session (0 other lines skipped); outcomes: completed 3

By request class
                                      cache  cache
  class           reqs  input output   read  write    hit      cost  time tools
  all                3     36    114   145K   4.6K  96.9%   $0.0832   14s  1.8s
  main               3     36    114   145K   4.6K  96.9%   $0.0832   14s  1.8s
...
```

`wye explain` shows what the server would do with a request, offline, by
calling the same functions in the same order:

```console
$ wye explain -c wye.toml --session session-a request.json
config: wye.toml
endpoint: POST /v1/messages
token: not required (no [auth])
body: 90 bytes (limit 32 MiB)
model: claude-opus-5-5; allowed by config: any; by token: any
route: upstream bedrock-us of 2 (cache_affine by session id)
failover: on 429, 529, connect to anthropic in that order, for bodies up to 8 MiB
upstream: POST https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages
provider key: x-api-key from $BEDROCK_API_KEY
model id: the body names anthropic.claude-opus-5-5 upstream, the provider's id for claude-opus-5-5
price: claude-opus-5-5 at bedrock-us: input $4.00, output $20.00, cache read $0.40, cache write 5m $5.00, cache write 1h $8.00 per million tokens (pricing version anthropic-2026-10-06)
decision: forward
```

## Features

- **APIs:** Anthropic Messages from Anthropic, Claude Platform on AWS, and
  Amazon Bedrock, using API keys; OpenAI Responses and Chat Completions. Streams
  are relayed event by event, never buffered.
- **Routing:** several weighted accounts per route, each session on one; failover
  before output.
- **Access control:** Wye's own Ed25519 tokens, or your identity provider's, with
  keys pinned in config. Model permissions and output-token ceilings. A token can
  only narrow what config allows.
- **Model changes:** aliases rolled out per team or token, and upgrade checks that
  report which clients a model's breaking changes would fail. Requests are never
  changed.
- **Visibility:** usage records, OTLP logs, traces (one per Claude Code session),
  and metrics; optional cache-miss diagnostics.
- **Tooling:** `wye explain` and `wye policy-test` check decisions offline and in
  CI, and `wye policy-diff` exits non-zero when a config change widens access.
  Also `wye init`, `wye service`, `wye validate`, `wye doctor`, `wye token`, and
  `wye probe`. Every command has `--help`, and reads `~/.wye/wye.toml` unless told
  otherwise (`-c`, `$WYE_CONFIG`).

Detail: [docs/product.md](docs/product.md#built).

## Performance

Wye is meant to never be the slow part. Every commit is benchmarked against
its parent, in alternating rounds on the same machine (`scripts/check.sh`),
and a change that makes Wye slower or heavier beyond the gate's tolerances is
not committed. The numbers
below are lab numbers from one laptop (Intel i9-9880H, macOS) against a local
mock provider, not production measurements.

| What Wye adds | Plain HTTP/1.1 | TLS + HTTP/2 |
|---|---|---|
| Latency per request (p50) | 0.28 ms | 0.22 ms |
| CPU per request | 120 µs | 161 µs |
| CPU per streamed token event | 18.5 µs | |
| Heap allocations per streamed event | 0 | |
| 2,000 concurrent token-paced streams, delay per event (p50 / p99) | | 1.4 / 4.0 ms |
| Memory per open stream at that load | | 9.3 KB |

Compared at their best configurations on the same machine, Bifrost used about
6.6× more CPU per request and 4.4× more per streamed event, and LiteLLM about
120× and 9×. KrakenD CE cannot stream at all, though it beats Wye on bulk
transfer and TLS uploads. [docs/research.md](docs/research.md#benchmarks) has
the method, the full tables, the configurations used, and where Wye loses.

## Status

Built and covered by tests: everything listed under [Features](#features).

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
- Translate between provider APIs, rewrite prompts, or cache responses.
- Run agents or tools.
- Grow a dashboard or an admin API. Config changes go through Git and a
  restart or rolling deploy.

## Development

```bash
git clone https://github.com/askrid/wye && cd wye
git config core.hooksPath .githooks   # run the full check before each commit
scripts/check.sh                      # fmt, clippy, tests, release build, benchmark vs HEAD
```

Tests use local mock upstreams and never call a paid API.
[docs/development.md](docs/development.md) explains the benchmark gate and CI.

## Documentation

[docs/README.md](docs/README.md) says which document answers what.
[examples/wye.toml](examples/wye.toml) lists every configuration option.

## License

[WTFPL](LICENSE). Do what you want with it.
