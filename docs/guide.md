# User guide

How to use Wye, by task, then an [FAQ](#faq). Reference lives elsewhere: [operations.md](operations.md) for the CLI, deployment, and usage records; [policy.md](policy.md) for tokens, permissions, and pricing; [examples/wye.toml](../examples/wye.toml) for every field.

- Set it up: [installing](#installing), [choosing a setup](#choosing-a-setup), [running it](#running-it)
- See what your agents used and spent: [reading `wye report`](#reading-wye-report)
- Share one gateway with a team: [using it for a team](#using-it-for-a-team)
- Connect your own apps: [using it from apps](#using-it-from-apps)
- Spread load across accounts and providers: [several upstreams](#several-upstreams)
- Put dollars on usage: [seeing costs](#seeing-costs)
- Move to a new model safely: [model aliases and upgrades](#model-aliases-and-upgrades)
- Send usage to Grafana, Datadog, or another [OpenTelemetry backend](#sending-usage-to-your-opentelemetry-backend)
- Change the config or Wye itself: [changing or upgrading](#changing-or-upgrading)
- Fix something: [troubleshooting](#troubleshooting)

## Installing

`cargo install` builds the latest release (Rust 1.99 or later); without `--tag`, the unreleased `main`:

```bash
cargo install --git https://github.com/askrid/wye --tag v0.1.0-rc.2 wye --locked
```

Linux and macOS archives are on the [releases page](https://github.com/askrid/wye/releases); `gh attestation verify <archive> --repo askrid/wye` shows the tag and commit one was built from ([SECURITY.md](../SECURITY.md#releases)).

## Choosing a setup

`wye init` writes a config, and next to it `policy-tests/` with sample requests that `wye policy-test` checks against it ([what they cover](operations.md#commands)). It never writes over an existing file, and prints the next steps. It writes one of three [setups](glossary.md#the-product):

| | The caller's own credential | A provider key the gateway holds |
|---|---|---|
| **This machine only** (loopback) | `wye init`: you, with your claude.ai login or API key | `wye init --provider-key`: you with a company key, or a sidecar beside one app |
| **Other machines** (gateway tokens) | not possible: a shared gateway cannot forward callers' credentials | `wye init --shared`: a team, or an app's services |

- **Passthrough** (`wye init`): the gateway holds no secret. Config: `~/.wye/wye.toml`.
- **Provider key** (`--provider-key`): an Anthropic API key in `anthropic.key` next to the config, read at startup. It replaces whatever a caller sends. Config: `~/.wye/wye.toml`.
- **Shared** (`--shared`): the gateway listens on every interface. The provider key comes from `ANTHROPIC_API_KEY` in the deployment. It creates the signing key `~/.wye/signing.key` (mode 600) unless one is there to reuse. Config: `~/.wye/wye.toml`; for a config repository, where it is reviewed, `-c wye.toml` there ([for a team](#using-it-for-a-team)).

`-c <path>` names another path. `--openai` adds `[routes.openai]` for Codex and the OpenAI SDKs, with the key in `openai.key` next to the config (with `--shared`, in `OPENAI_API_KEY`).

With passthrough, a login counts against your plan and an API key bills the API. As without Wye, an exported `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` takes over from your login.

## Running it

In the background, as a LaunchAgent on macOS or a systemd user unit on Linux:

```bash
wye service install     # shows what it sets up and asks
wye service status
wye service uninstall   # the config, the usage log, and the message log stay
```

A service does not inherit your shell's environment, so it refuses a config with secrets in environment variables; `wye init` and `wye init --provider-key` write none ([running as a service](operations.md#running-as-a-service)).

In a terminal:

```bash
wye serve --usage-log ~/.wye/usage.jsonl   # messages go to stderr
```

Then start your agent with `wye claude` or `wye codex`, which set its base URL and leave its own settings alone. `wye claude --bg "task"` starts a background session that goes through the gateway too.

## Reading `wye report`

`wye report` summarizes `~/.wye/usage.jsonl` and its rotated files, or the files you name (`-` for standard input), with no OpenTelemetry backend; `--since 7d` keeps the last week ([options](operations.md#commands)). The [README](../README.md#quick-start) shows a sample.

- **First lines:** requests, sessions, the window if one was given, lines skipped if any, outcomes, error statuses, and upstream errors by kind (`connect`, `timeout`, `reset`). A 429 [rejection](glossary.md#requests-and-where-they-go) is an answer, so its outcome reads `completed`, and its status is listed ([outcomes](operations.md#usage-records-and-telemetry)).
- **By [request class](glossary.md#usage-and-cost), by [upstream](glossary.md#requests-and-where-they-go)** (with two or more): tokens, cache-hit rate (cache reads as a share of prompt tokens), cost with `[pricing]`, and time in the model and in tools. Requests without a class hint, such as an SDK's, are `(unclassified)`. Cost is marked `+` when it includes partial or unknown costs, or records written without `[pricing]`.
- **Cache misses**, when there are any: a cache write with nothing read. The first write of a session or a subagent is expected, as are compactions and the request after one; `count_tokens` and refused requests before it do not count. Unexplained ones are worth a look; [cache diagnostics](architecture.md#cache-diagnostics) can name the cause.
- **Failovers, upgrade issues:** when records have them.
- **Tools:** calls the model requested, and calls and time Claude Code reported.
- **Top sessions and prompts:** the ten with the most tokens.

Input lines that are not usage records are skipped and counted. Logs of every record format version are read alike.

## Using it for a team

No image is published yet, so build one from a clone of Wye. Then, in your config repository:

```bash
docker build -t wye .                               # in the clone
wye init --shared -c wye.toml                       # and ~/.wye/signing.key
wye token issue alice -c wye.toml --out alice.token # valid for a week
docker run -p 8080:8080 -v ./wye.toml:/etc/wye/wye.toml:ro -e ANTHROPIC_API_KEY wye
```

Put it behind HTTPS, which `wye claude` and `wye codex` require of a remote gateway: a load balancer, an ingress, or Caddy, which gets and renews a certificate by itself ([example](operations.md#deployment)). Each developer then runs:

```bash
export WYE_URL=https://wye.example.com WYE_TOKEN=<their token>
wye claude
```

- Without installing Wye, Claude Code takes `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`.
- `wye token issue --models` and `--max-output-tokens` narrow what one token may do; it can never widen the config ([gateway tokens](policy.md#gateway-tokens)).
- A token cannot be revoked, only outlived, so keep lifetimes short ([revocation](policy.md#revocation)). `apiKeyHelper` and CI identity avoid pasting tokens ([token renewal](operations.md#token-renewal)).
- `wye codex` needs `[routes.openai]`: `wye init --shared --openai`, and `-e OPENAI_API_KEY` for the container.
- `wye policy-diff` in CI flags a config change that widens access.
- [examples/team/](../examples/team) is a complete team config with its policy tests: developers through SSO, CI jobs through GitHub Actions tokens, two Anthropic keys with failover, prices, and telemetry.

## Using it from apps

The official SDKs read their base URL and key from the environment, so an app changes no code:

```bash
ANTHROPIC_BASE_URL=https://wye.example.com ANTHROPIC_API_KEY=<key> python app.py
OPENAI_BASE_URL=https://wye.example.com/v1 OPENAI_API_KEY=<key> node app.js
```

Wye stands between your backend and the provider, never in front of your users: they sign in to your backend, which keeps their authentication and quotas, and only it calls Wye. Two places to run it:

- **Beside the app** (`wye init --provider-key -c <path>`): a sidecar on the same host or pod, on loopback, holding the provider key. No tokens to manage; without `[auth]`, records name no subject, so there are no per-service model permissions, output-token ceilings, or canaries by subject. A sidecar has [not yet run in production](operations.md#deployment).
- **One internal gateway for several services** (`wye init --shared`): each service gets its own gateway token, so permissions, ceilings, aliases, and usage are per service; deploy it as in [using it for a team](#using-it-for-a-team).

The app's API key, by setup:

- Shared, or any config with `[auth]`: its gateway token, which Wye reads from `x-api-key` or `Authorization: Bearer`.
- Passthrough: the app's own API key, which goes through.
- Provider key: any non-empty placeholder, since the SDKs refuse to send a request without one.

Streamed chat completions report usage only with `stream_options={"include_usage": True}`, which Wye does not add.

[examples/app/](../examples/app) is a complete config for a product's backend, with its policy tests: a gateway token per service, model aliases with a canary, Anthropic with Claude Platform on AWS as failover, OpenAI, an upgrade check, and prices.

## Several upstreams

When one provider key's rate limits are not enough, or spend is split across accounts, list each provider key as its own `[[routes.claude.upstreams]]` entry. An upstream is named by its provider (`anthropic`, `bedrock`) unless it has an `id`, which two of one provider each need:

```toml
[routes.claude]
failover = ["429", "529", "connect"]

[[routes.claude.upstreams]]
id = "key-a"
weight = 2                     # twice the sessions of key-b
provider_key_env = "ANTHROPIC_KEY_A"

[[routes.claude.upstreams]]
id = "key-b"
provider_key_env = "ANTHROPIC_KEY_B"
```

- Each session stays on one upstream, chosen from its session id (SDK callers without one: from the gateway token's subject), so its prompt cache keeps hitting ([cache affinity](architecture.md#cache-affinity)). Without either, as for a local app with no gateway token, every request goes to the same upstream.
- Weights share sessions, not tokens. Adding an upstream moves only the sessions it now wins; removing one moves only its own, and their next turn writes the cache again at the new upstream.
- With `failover`, a [rejected](glossary.md#requests-and-where-they-go) request goes to the session's next upstream in the same turn, never after output has started ([failover](architecture.md#failover)). Without it, a `429` goes back to the client, whose retry reaches the same upstream.
- `select = "ordered"` makes the first upstream primary and the others standby.
- Claude Platform on AWS and Bedrock mix in with `provider` ([examples/wye.toml](../examples/wye.toml)), and `[routes.openai]` takes several OpenAI upstreams the same way.
- A route of several upstreams holds their provider keys, so it does not combine with passthrough, which needs a route with one upstream. Under `wye service`, give each provider key with `provider_key_file` instead of `provider_key_env` ([why](operations.md#running-as-a-service)).
- `wye explain --session <id>` shows which upstream a session uses; records, OTLP, and `wye report` split usage and cost by `upstream`.

## Seeing costs

`wye init` writes no prices: copy the `[pricing]` block from [policy.md](policy.md#price-table) into your config, check each price against the provider's pricing page, and restart. Every record then carries `cost_usd`, and `wye report` a cost column.

- On a claude.ai plan, the cost is what the same tokens would cost on the API, not what you pay.
- Prices can differ per upstream (Bedrock, a region): an entry with `upstreams = [...]` applies only there.

## Model aliases and upgrades

Clients send an [alias](glossary.md#models), so moving every client to a new model is one reviewed change. Before you move, upgrade checks report which live Anthropic Messages requests the new model would reject:

```toml
[[aliases]]
name = "claude-coding"                # what clients send as their model
model = "claude-sonnet-5"             # what runs today
when = [{ subjects = ["team-canary-*"], model = "claude-sonnet-5-5" }]

[upgrade_check]
targets = ["claude-sonnet-5-5"]
```

1. Point clients at the alias: Claude Code with `ANTHROPIC_MODEL=claude-coding`, an app with `model="claude-coding"`. Claude Code's model picker lists only names containing `claude` or `anthropic`.
2. Read `wye report`'s upgrade issues, or `wye explain --target-model claude-sonnet-5-5 request.json` for one request. Fix what they name in the client; Wye never changes a request to fit.
3. Move a canary group first with `when` (it matches token subjects, so it needs `[auth]`), then everyone by changing `model`.

Records show both the requested and the served model, and permissions apply to the served one. [How it works](architecture.md#model-aliases-and-upgrade-checks).

## Sending usage to your OpenTelemetry backend

Add `[telemetry]` with an OTLP/HTTP endpoint, a collector or a backend that takes OTLP directly:

```toml
[telemetry]
otlp_endpoint = "http://localhost:4318"
otlp_headers_env = "OTLP_HEADERS"     # "key=value,...": a hosted backend's API key
```

Each request becomes a log record and a span, each Claude Code session one trace, and metrics count them ([what is exported](operations.md#usage-records-and-telemetry)). Records still go to the usage log. Under `wye service`, OTLP headers need a service definition of your own ([running as a service](operations.md#running-as-a-service)).

## Changing or upgrading

Edit the config or install a new `wye`, run `wye doctor`, then `wye service restart`, or restart `wye serve` in a terminal. What a restart checks and how long it waits for open streams: [running as a service](operations.md#running-as-a-service).

## Troubleshooting

Run `wye doctor` first. It checks secrets, files, and upstreams, lists every problem, and bills nothing.

Startup failures and the gateway's messages go to stderr, the message log (`~/.wye/wye.log`) under the service. Request failures are not logged there. Each request's `status` and `outcome` are in the usage log. A `502` or `504` of Wye's own records why in `upstream_error` (`connect`, `timeout`, or `reset`), which `wye report` counts ([fields](operations.md#usage-records-and-telemetry)).

`wye explain request.json` shows what the gateway would do with one request, offline, and why it would refuse.

- `no gateway at http://127.0.0.1:8080`: nothing answers `/healthz` there. Run `wye service status`, then read the end of `~/.wye/wye.log`; or start one with `wye serve`; or name another with `--url` or `WYE_URL`.
- `cannot listen on ...: the port is in use`: another gateway, perhaps. Stop it, or change `server.listen` and set `WYE_URL`.
- `401 invalid or missing gateway token`: one message for every gateway-token failure, on purpose. `wye explain --token-file <file> request.json` says which: expired, wrong audience, unknown key. Any other `401` is the provider's.
- `403 permission_error`: the gateway token may not use that model, or the request asks for more than its output-token ceiling. Run `wye explain`; see [permissions](policy.md#model-permissions).
- `404 no route serves ...`: no route for that API. Add the section it names, such as `[routes.openai]`.
- `408 request body stalled`: while Wye read `model`, the client sent nothing for 60 s, or had not reached it after 5 minutes. Check the client's network.
- `415 the request body must...`: a compressed or non-UTF-8 body during [the hold](glossary.md#requests-and-where-they-go). Send plain UTF-8 JSON.
- `529 overloaded_error` (`503` on the OpenAI APIs): from Wye, it says `gateway is...`: past `max_concurrent_requests` or `max_total_held_bytes`. Otherwise it comes from the provider. Clients retry on their own; if it persists, raise the limit.
- One `429` at each session start: Claude Code's probe on a claude.ai login; Anthropic answers it with or without Wye. Nothing to do.

[Failure behavior](operations.md#failure-behavior) lists the rest.

## FAQ

### Does it change my requests?

Only `model`, when config maps an alias or the provider has its own id, and the headers any gateway replaces (hop-by-hop, and the caller's authorization); a request Wye cannot allow is refused, never altered ([forwarding contract](architecture.md#forwarding-contract)).

### Does it see my prompts?

They pass through, and Wye reads only the fields its policy and records need; records never contain prompts, completions, or credentials ([usage records](operations.md#usage-records-and-telemetry)).

### What does it store?

Nothing between requests; its only files are the ones you give it ([files](operations.md#files)).

### Why not a config in the working directory?

A config decides where your credentials go, so Wye reads one only when you name it ([files](operations.md#files)).

### Does my subscription work?

Yes, for Claude Code on a claude.ai plan (Pro or Max) with the passthrough setup (`wye init`), on this machine only ([caller-credential passthrough](policy.md#caller-credential-passthrough)).

### Can I cap what a team spends?

Per request, with an output-token ceiling; across requests, not in Wye, which keeps no counters, and provider-held budgets are [planned](roadmap.md#m2-cost-policy-for-claude).

