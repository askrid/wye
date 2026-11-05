# <img src="site/assets/icon-fork.svg" alt="" width="36" align="absmiddle"> Wye

[![ci](https://img.shields.io/github/actions/workflow/status/askrid/wye/ci.yml?branch=main&style=flat-square&label=ci&labelColor=151515)](https://github.com/askrid/wye/actions/workflows/ci.yml) [![release](https://img.shields.io/github/v/release/askrid/wye?include_prereleases&style=flat-square&label=release&labelColor=151515&color=f2b544)](https://github.com/askrid/wye/releases) [![license](https://img.shields.io/github/license/askrid/wye?style=flat-square&labelColor=151515&color=3a3a3a)](LICENSE)

**LLM gateway without bloat.**

Small, fast, and stateless: one binary, one TOML file, under a millisecond per request ([p50 on c7i.4xlarge](#performance)), and the features a gateway needs, no more.

## Quick start

Prerelease, tested on one machine; [status](#status) lists what is not yet verified against live providers.

```bash
cargo install --git https://github.com/askrid/wye --tag v0.1.0-rc.2 wye --locked   # Rust 1.99+

wye init              # writes ~/.wye/wye.toml; needs no key
wye service install   # shows what it sets up and asks; runs the gateway in the background
wye claude            # Claude Code through the gateway, signed in as usual
wye report            # what it recorded
```

Passthrough, the default setup, forwards the caller's own claude.ai login or API key, listens on this machine only, and holds no secret. Prebuilt archives for Linux and macOS are on the [releases page](https://github.com/askrid/wye/releases); `gh attestation verify <archive> --repo askrid/wye` checks that this repository's release workflow built one. The [guide](docs/guide.md) covers the other setups: a gateway that holds a provider key, a shared gateway behind HTTPS for a team or an app's services, upgrades, and troubleshooting.

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

- **Shows usage and cost.** One JSON line per request: tokens, cost, model, and for Claude Code and Codex the session, subagent, request class, and tool calls. `wye report` reads the usage log, no backend needed. OTLP sends logs, metrics, and one trace per Claude Code session to yours. Every prompt-cache miss is recorded with its reason (`compaction`, `after_compaction`, or `unexplained`). ([reading the report](docs/guide.md#reading-wye-report), [telemetry](docs/operations.md#usage-records-and-telemetry), [prices](docs/policy.md#price-table))
- **Gives each caller its own gateway token.** Instead of the provider key, signed by Wye or by your identity provider (GitHub Actions, or SSO through Claude Code's `apiKeyHelper`). A token names its models and its output-token ceiling; anything else is refused before the provider gets the whole request. With `[discovery]` and `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`, Claude Code's model picker shows only those models. ([teams](docs/guide.md#using-it-for-a-team), [apps](docs/guide.md#using-it-from-apps), [tokens](docs/policy.md#gateway-tokens))
- **Spreads load across keys and providers.** Several Anthropic keys, Claude Platform on AWS, and Bedrock behind one address (several OpenAI keys too), split by weight, with failover between them. Each key or account is an [upstream](docs/glossary.md#requests-and-where-they-go). ([several upstreams](docs/guide.md#several-upstreams))
- **Changes models in one place.** Aliases map a name to a model, per gateway-token subject (a person, a repository, or a service). Upgrade checks tell you which Messages requests a newer model would reject, before you switch. Wye never rewrites a request to make it fit. ([aliases and upgrades](docs/guide.md#model-aliases-and-upgrades))
- **Checks decisions before they ship.** `wye explain` shows what the gateway would do with a request, offline. `wye policy-test` and `wye policy-diff` run in CI and fail when a change widens access. `wye doctor` checks secrets, files, and upstreams, and bills nothing. ([commands](docs/operations.md#commands))
- **Works with existing clients.** Point Claude Code, Codex, or any app on the official Anthropic or OpenAI SDK at it by base URL; `wye claude` and `wye codex` do it for you. Messages, Responses, and Chat Completions go through untranslated, except a model id your config maps. Streams are relayed as the provider sends them, never buffered. ([running it](docs/guide.md#running-it), [apps](docs/guide.md#using-it-from-apps))
- **Runs as one process, or many.** `wye service` installs a macOS LaunchAgent or a systemd user unit. Replicas behind any load balancer never talk to each other, and each sends a session to the same upstream, so prompt caches stay warm without sticky sessions. Each has health checks and a graceful shutdown. ([deployment](docs/operations.md#deployment))

## Performance

Lab results from three fresh AWS `c7i.4xlarge` hosts, each running Wye and four other gateways in the same session against a mock provider, every gateway pinned to the same two physical cores; one command reproduces them ([reference environment](docs/benchmarks.md#reference-environment)). Each cell is the gateway's median divided by Wye's (TLS to the upstream; stream events with 2,000 token-paced streams):

| Gateway | Added latency (p50) | CPU per request | CPU per streamed event | Memory after load |
|---|---|---|---|---|
| [agentgateway](https://github.com/agentgateway/agentgateway) 1.7 (Rust) | 2.8× | 1.9× | 1.2× | 2.1× |
| [Bifrost](https://github.com/maximhq/bifrost) 2.3 (Go) | 9.7× | 8.1× | 4.0× | 28× |
| [LiteLLM](https://github.com/BerriAI/litellm) 1.105 (Python) | 330× | 220× | 9.8× | 19× |
| [KrakenD](https://github.com/krakend/krakend-ce) 3.0 CE (Go) | 1.4× | 1.7× | cannot stream | 1.4× |

Under load the gap widens. With 2,000 token-paced streams, Wye adds 0.28 ms per event at p99; agentgateway adds 1.0 ms, Bifrost 132 ms, LiteLLM 490 ms, and KrakenD, which buffers, 404 ms ([results](docs/benchmarks.md#results)). Under the stress scenarios LiteLLM failed 104-132 requests and streams on every host; Wye failed none, in any scenario ([failures](docs/benchmarks.md#failures)).

Where others lead: KrakenD on bulk transfer (3.7 against 2.4 GB/s), because it buffers responses instead of streaming them; agentgateway on cold connections and on memory beside clients that stopped reading, with a delay per event close to Wye's; LiteLLM on memory per idle stream over plain HTTP, on top of 2.1 GB at idle. Each vendor's own claim beside the number measured here, the method, and the configs: [docs/benchmarks.md](docs/benchmarks.md#what-they-claim-what-i-measured).

| What Wye adds | Plain HTTP/1.1 | TLS + HTTP/2 |
|---|---|---|
| Latency per request (p50 / p99) | 254 / 559 µs | 325 / 628 µs |
| CPU per request | 42 µs | 62.5 µs |
| 2,000 concurrent token-paced streams, delay per event (p50 / p99) | | 77 / 282 µs |
| CPU per event and memory per open stream at that load | | 14 µs, 12.6 KB |
| Memory at idle | 28 MB | 28 MB |

A heavy Claude Code user (five concurrent streams) takes about 0.4% of a vCPU and 270 KB of memory ([sizing](docs/operations.md#sizing-estimate), an estimate). The delay per event doesn't add up over a stream, so tokens per second don't change.

Every commit that changes code is benchmarked against its parent, in alternating rounds on the same machine (`scripts/check.sh`). If it makes Wye slower or heavier beyond the commit gate's tolerances, it doesn't get in.

## How it compares

Two choices explain most answers: each provider's native API, passed through, and no state between requests.

| Teams usually want | Wye |
|---|---|
| Spend by team, user, and app | Yes. Down to the agent session, subagent, and tool, with prompt-cache misses explained |
| Access per caller instead of the provider key | Yes. A gateway token per developer, CI job, or app |
| Spend caps and rate limits | Partly. Per-request output-token ceilings, and the provider's own limits; Wye keeps no counters. Provider-held budgets per team are [planned](docs/roadmap.md#m2-cost-policy-for-claude) |
| Failover and load across keys | Yes. Split by weight ([configuration](#configuration)) |
| Logs, metrics, and traces | Yes. In your OpenTelemetry backend |
| Switching models centrally | Yes. Aliases and upgrade checks |
| Policy you can review and test | Yes. One TOML file in Git, checked in CI by `wye policy-test` and `wye policy-diff` |
| One API for every provider | No. For many providers behind one API, LiteLLM or OpenRouter fit better |
| Guardrails, redaction, response caching | No. [What Wye will not do](#what-wye-will-not-do) says why |
| Scale out by adding replicas | Yes. Behind any load balancer. The provider keys' rate limits are the ceiling; more keys raise it |

A team that runs only Claude Code with SSO may prefer Anthropic's own self-hosted gateway for Claude Code, which follows every Claude Code release.

Why native APIs: provider APIs keep changing. Within one year on Anthropic's API, `thinking.budget_tokens`, sampling parameters, assistant prefill, forced `tool_choice`, and `output_format` all broke on newer models. A gateway that translates everything into one format is always catching up ([what Wye changes](docs/architecture.md#forwarding-contract)).

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

Each session stays on one upstream. A rejected request moves to the other; two keys of one provider work the same way ([details](docs/guide.md#several-upstreams)). Secrets come from environment variables or files, never the config. An unknown field stops startup. [examples/wye.toml](examples/wye.toml) has every field with its default and limits, and a test keeps it valid.

`wye explain` shows what this config decides for one request, offline, through the gateway's own functions:

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

- Count toward global rate limits, spending budgets, or quotas: that needs shared state; providers already enforce them per key or workspace.
- Break circuits, cool down upstreams, or balance by measured latency: each remembers across requests. Failover handles a rejection within the request.
- Translate between provider APIs, rewrite prompts, swap in another model when one fails, or cache responses.
- Filter content (guardrails, redaction) or store prompts.
- Terminate TLS: a load balancer, an ingress, or Caddy in front does it, and renews the certificate.
- Run agents or tools.
- Protect a public endpoint from abuse: per-user authentication and quotas stay in your backend.
- Grow a dashboard or an admin API. Config changes go through Git and a restart or rolling deploy.

## Documentation

[docs/README.md](docs/README.md) maps each question to the document that answers it, and [docs/glossary.md](docs/glossary.md) defines the terms. Development: [docs/development.md](docs/development.md); tests run against local mock upstreams and never call a paid API. License: [MIT](LICENSE).
