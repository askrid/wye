# Wye

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

Build and install from source (the toolchain is pinned in
`rust-toolchain.toml`):

```bash
cargo install --git https://github.com/askrid/wye wye --locked
```

Prebuilt Linux and macOS archives come with the first release, which is not
published yet.

### For yourself

With a claude.ai subscription (Pro or Max), Wye forwards your own Claude Code
login, on this machine only:

```bash
wye init --subscription           # writes ~/.wye/wye.toml
wye serve 2>>~/.wye/usage.jsonl   # the gateway; its stderr is the usage log
wye claude                        # in another terminal: Claude Code through it
wye report                        # what it recorded
```

With an Anthropic API key, run plain `wye init`, put the key in
`~/.wye/anthropic.key` (`chmod 600`), and check it with `wye doctor`.

`wye claude` starts Claude Code with its base URL set to the gateway and its
gateway hints on (request class, prompt id, and tool times, for the usage
log), and `wye claude --bg "task"` starts a background session that goes
through it too. It does not touch your Claude Code settings. To keep the
gateway running across logins, install it as a service
([docs/operations.md](docs/operations.md#running-as-a-service)).

### For a team

One gateway for every developer and service, deployed from a config
repository:

```bash
wye init --auth --out wye.toml     # [auth], and a signing key in ~/.wye/issuer.key
wye token issue alice > alice.token  # a gateway token, valid for a week
docker run -p 8080:8080 -v ./wye.toml:/etc/wye/wye.toml:ro -e ANTHROPIC_API_KEY wye
```

Each developer then points their agent at it:

```bash
export WYE_URL=https://wye.example.com WYE_TOKEN=<their token>
wye claude                         # or: wye codex
```

Without installing Wye, Claude Code takes `ANTHROPIC_BASE_URL` and
`ANTHROPIC_AUTH_TOKEN`, or an `apiKeyHelper` that fetches short-lived tokens
from your identity provider
([docs/operations.md](docs/operations.md#credential-renewal)).

### For apps

The official SDKs read their base URL and key from the environment, so an app
changes no code:

```bash
ANTHROPIC_BASE_URL=https://wye.example.com ANTHROPIC_API_KEY=<gateway token> python app.py
OPENAI_BASE_URL=https://wye.example.com/v1 OPENAI_API_KEY=<gateway token> node app.js
```

The OpenAI APIs need `[routes.openai]` in the config (`wye init --openai`).
Without `[auth]`, the key can be any placeholder, and Wye sends its own
provider key instead.

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

Each Claude Code session is pinned to one account by hashing its session id,
so its prompt cache stays warm, and every replica picks the same account. If
that account answers `429` or `529`, or the connection fails before the request
body was delivered, Wye sends the same request to the next account in the same
turn. It never does this after the first response byte, and it relies on
`429` and `529` meaning the provider did not process the request (not yet
confirmed for every provider).

Secrets are read from environment variables or files and never appear in the
config. Unknown fields are rejected at startup.
[examples/wye.toml](examples/wye.toml) is the reference: every option with its
default and limits, kept valid by a test.

## What it looks like

Each request produces one JSON line on stderr (and, with `[telemetry]`, an
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

Costs are exact decimal strings. Prices are parsed into integer pico-dollars
and summed with checked arithmetic, so there is no floating point anywhere
near money. A model with no price is recorded as `unknown`, never as $0.

`wye report` summarizes a log without any backend:

```console
$ wye report
3 requests in 1 session (2 other lines skipped); outcomes: completed 3

By request class
  class                           reqs        input     output    cache read   cache write     hit        cost       time      tools
  all                                3           36        114        144630          4590   96.9%     $0.0832       0.0s       0.0s
  main                               3           36        114        144630          4590   96.9%     $0.0832       0.0s       0.0s
...
```

`wye explain` shows what the server would do with a request, offline, by
calling the same functions in the same order:

```console
$ wye explain --session session-a request.json
endpoint: POST /v1/messages
credential: not required (no [auth])
body: 90 bytes (limit 32 MiB)
model: claude-opus-5-5; allowed by config: any; by credential: any
route: upstream bedrock-us of 2 (cache_affine by session id)
failover: on 429, 529, connect to anthropic in that order, for bodies up to 8 MiB
upstream: POST https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages
provider credential: x-api-key from $BEDROCK_API_KEY
model id: the body names anthropic.claude-opus-5-5 upstream, the provider's id for claude-opus-5-5
price: claude-opus-5-5 at bedrock-us: input $4.00, output $20.00, cache read $0.40, cache write 5m $5.00, cache write 1h $8.00 per million tokens (pricing version anthropic-2026-10-06)
decision: forward
```

## Features

Providers and APIs
- Anthropic Messages (`/v1/messages`, `count_tokens`) from Anthropic, Claude
  Platform on AWS, and Amazon Bedrock, using API keys.
- OpenAI Responses and Chat Completions.
- `GET /v1/models` answered from config, filtered by what the caller may use.
- Streams are relayed event by event, never buffered.

Routing
- Several accounts per route, weighted, with each session pinned to one.
- `ordered` selection for a primary account with standbys.
- Failover before output, with the request body kept within per-request and
  process-wide memory limits.

Access control
- Wye's own Ed25519 credentials (compact JWTs with the algorithm pinned), or
  tokens from your identity provider such as GitHub Actions OIDC or your SSO,
  with keys pinned in config.
- Model permissions and output-token ceilings. A credential can only narrow
  what config allows, never widen it.

Model changes
- Aliases (`coding-default` → a model), rolled out per team or credential.
- Upgrade checks: a sample of live requests is checked against the next
  model's breaking changes, and Wye reports which clients would fail. It never
  changes the requests.

Visibility
- Usage records with tokens, cost, cache hits and misses, upstream, failovers,
  session, subagent, request class, and tool calls and time.
- OTLP logs, traces (one per Claude Code session), and metrics.
- Optional cache-miss diagnostics from keyed hashes of the prompt sections.

Tooling
- `wye explain` for one request, and `wye policy-test` for a directory of
  sample requests with expected decisions, so routing and permission changes
  can be tested in CI.
- `wye policy-diff old.toml new.toml` exits non-zero when a config change
  widens access: a model newly allowed, a ceiling raised, a longer credential
  lifetime, a new trusted key, auth removed, a listener opened wider, a new API route, or the operator's
  key where callers brought their own.
- `wye init` for one developer or a team, `wye validate` and `wye doctor`,
  `wye token` to issue gateway tokens, and `wye probe` (a health check for
  images without curl). Every command has `--help`, and finds its config
  without flags (`./wye.toml`, then `~/.wye/wye.toml`).

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

## Running it

A Docker image builds from the repository:

```bash
docker build -t wye .
docker run -p 8080:8080 -v ./wye.toml:/etc/wye/wye.toml:ro -e ANTHROPIC_API_KEY wye
```

Inside a container Wye has to listen on `0.0.0.0`, and Wye refuses to listen
beyond loopback without `[auth]`. Health checks are `/healthz` and `/readyz`.
On shutdown, readiness fails first and in-flight streams get a grace period to
finish. [docs/operations.md](docs/operations.md) covers deployment with and
without Kubernetes, telemetry, and credential renewal.

## Development

```bash
git clone https://github.com/askrid/wye && cd wye
git config core.hooksPath .githooks   # run the full check before each commit
scripts/check.sh                      # fmt, clippy, tests, release build, benchmark vs HEAD
```

The check takes about eight minutes, most of it the benchmark. Tests run
against local mock upstreams and never call a paid API. Pull requests also
run CI on GitHub: lints and tests on Linux and macOS, `cargo deny`, and the
benchmark against the base branch, gating allocations, failed requests, and
failed streams (shared runners are too noisy to gate latency).
[docs/development.md](docs/development.md) explains the benchmark gate, and
how to compare two builds when a change is too small for the gate to see.

The code is split into `crates/gateway-core` (config, policy, routing, usage
parsing, pricing; no I/O) and `crates/wye` (the server and the CLI).
`crates/wye-bench` is the benchmark and `crates/wye-testkit` holds the test
helpers.

## Documentation

- [docs/README.md](docs/README.md): which document answers what, and the terms they use
- [docs/product.md](docs/product.md): scope, who it is for, strengths and weaknesses
- [examples/wye.toml](examples/wye.toml): every configuration option
- [docs/architecture.md](docs/architecture.md): how it works
- [docs/policy.md](docs/policy.md): credentials, permissions, pricing
- [docs/operations.md](docs/operations.md): CLI, deployment, telemetry
- [docs/roadmap.md](docs/roadmap.md): status and what is next
- [docs/research.md](docs/research.md): benchmarks, experiments, open questions

## License

[WTFPL](LICENSE). Do what you want with it.
