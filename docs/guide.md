# User guide

How to use Wye, by task, then an [FAQ](#faq). Reference lives elsewhere: [operations.md](operations.md) for the CLI, deployment, and telemetry; [policy.md](policy.md) for tokens, permissions, and pricing; [examples/wye.toml](../examples/wye.toml) for every option.

## Choosing a setup

Install with `cargo install --git https://github.com/askrid/wye wye --locked` (Rust 1.98 or later). Prerelease archives for Linux and macOS are on the [releases page](https://github.com/askrid/wye/releases).

`wye init` writes a config, never over an existing file, and prints the next steps. It has three modes:

| Command | For | Credentials | Config |
|---|---|---|---|
| `wye init` | You, on this machine | Each caller's own credential goes through: Claude Code's claude.ai login, or an API key. The gateway holds no secret | `~/.wye/wye.toml` |
| `wye init --api-key` | You, with a key the gateway holds | An Anthropic API key in `anthropic.key` next to the config, read at startup. It replaces whatever a caller sends | `~/.wye/wye.toml` |
| `wye init --team` | A shared gateway | Callers present gateway tokens. The provider key comes from `ANTHROPIC_API_KEY` in the deployment. Creates the signing key `~/.wye/issuer.key` (mode 600) unless one is there to reuse | `./wye.toml`, for the repository it is reviewed in |

The first two listen on loopback only; `--team` listens on every interface. `--out` names another path. `--openai` adds `[routes.openai]` for Codex and the OpenAI SDKs, with the key in `openai.key` next to the config (with `--team`, in `OPENAI_API_KEY`).

With the default, a login counts against your plan and an API key bills the API. As without Wye, an exported `ANTHROPIC_API_KEY` takes over from your login.

## Running it

In the background, as a LaunchAgent on macOS or a systemd user unit on Linux:

```bash
wye service install     # shows what it sets up and asks
wye service status
wye service uninstall   # the config and the log stay
```

A service does not inherit your shell's environment, so it refuses a config with secrets in environment variables; `wye init` and `wye init --api-key` write none. [Running as a service](operations.md#running-as-a-service) has the detail.

In a terminal:

```bash
wye serve >> ~/.wye/usage.jsonl
```

Usage records go to stdout and messages to stderr, and `~/.wye/usage.jsonl` is where `wye report` looks.

Then start your agent with `wye claude` or `wye codex`, which set its base URL and leave its own settings alone. `wye claude --bg "task"` starts a background session that goes through the gateway too. `wye claude --help` has the options.

## Reading `wye report`

`wye report` summarizes `~/.wye/usage.jsonl`, or a file you name (`-` for standard input), with no telemetry backend. The [README](../README.md) shows a sample.

- **First lines:** requests, sessions, outcomes, and error statuses. A 429 refusal is an answer, so its outcome reads `completed`, and its status is listed.
- **By request class, by upstream:** tokens, cache-hit rate (cache reads as a share of prompt tokens), request time, tool time, and cost with `[pricing]`. Cost is marked `+` when it includes partial or unknown costs, or records written without `[pricing]`.
- **Cache misses:** a cache write with nothing read. The first write of a session or a subagent is expected, as are compactions and the request after one; `count_tokens` and refused requests before it do not count. Unexplained ones are worth a look; [cache diagnostics](research.md#2-cache-miss-diagnostics) can name the cause.
- **Failovers, upgrade issues:** when records have them.
- **Tools:** calls the model requested, and calls and time Claude Code reported.
- **Top sessions and prompts:** the ten with the most tokens.

Other lines are skipped and counted.

## Using it for a team

No image is published yet, so build one from a clone of Wye. Then, in your config repository:

```bash
docker build -t wye .                               # in the clone
wye init --team                                     # ./wye.toml and ~/.wye/issuer.key
wye token issue alice -c wye.toml --out alice.token # valid for a week
docker run -p 8080:8080 -v ./wye.toml:/etc/wye/wye.toml:ro -e ANTHROPIC_API_KEY wye
```

Put it behind HTTPS: Wye speaks plain HTTP, and `wye claude` and `wye codex` refuse a remote gateway over plain http ([deployment](operations.md#deployment-modes)). Each developer then runs:

```bash
export WYE_URL=https://wye.example.com WYE_TOKEN=<their token>
wye claude
```

- Without installing Wye, Claude Code takes `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`.
- `wye token issue --models` and `--max-output-tokens` narrow what one token may do; it can never widen the config ([gateway tokens](policy.md#gateway-tokens)).
- A token cannot be revoked, only outlived, so keep lifetimes short ([revocation](policy.md#revocation)). `apiKeyHelper` and CI identity avoid pasting tokens ([token renewal](operations.md#token-renewal)).
- `wye codex` needs `[routes.openai]`: `wye init --team --openai`, and `-e OPENAI_API_KEY` for the container.
- `wye policy-diff` in CI flags a config change that widens access.

## Using it from apps

The official SDKs read their base URL and key from the environment, so an app changes no code:

```bash
ANTHROPIC_BASE_URL=https://wye.example.com ANTHROPIC_API_KEY=<key> python app.py
OPENAI_BASE_URL=https://wye.example.com/v1 OPENAI_API_KEY=<key> node app.js
```

The key depends on the gateway:

- With `[auth]`: a gateway token, which Wye reads from `x-api-key` or `Authorization: Bearer`.
- The default `wye init`: the app's own provider key, which goes through.
- A gateway that holds a key (`wye init --api-key`): any non-empty placeholder, since the SDKs refuse to send a request without a key.

Streamed chat completions report usage only with `stream_options={"include_usage": True}`, which Wye does not add.

## Changing or upgrading

- **The config:** edit it, run `wye doctor`, then `wye service restart`. A config that would not start is refused, and the running service stays as it was. In a terminal, restart `wye serve`.
- **A new `wye` at the same path:** `wye service restart`.
- **Another config, or a `wye` at another path:** `wye service install` again (`-c <path>`). It replaces the definition.

Either way the old gateway finishes its requests first, for up to the [stop timeout](operations.md#running-as-a-service) (35 s by default), and new connections are refused until then. Claude Code retries streams cut off after that.

Models: [aliases and upgrade checks](architecture.md#model-aliases-and-upgrade-checks) move a model by one config change, after reporting which clients would fail.

## Troubleshooting

Run `wye doctor` first. It checks keys, files, and endpoints, lists every problem, and bills nothing.

**Where errors go.** Startup failures and the gateway's messages go to stderr: `~/.wye/wye.log` under the service. Request failures are not logged there. Each request's `status` and `outcome` are in the usage log, and `wye report` lists error statuses. A `502` does not yet record why ([open](roadmap.md#open-findings)).

`wye explain request.json` shows what the server would do with one request, offline, and why it would refuse.

| You see | It means | Do |
|---|---|---|
| `no gateway at http://127.0.0.1:8080` | Nothing answers `/healthz` there | `wye service status`, then the end of `~/.wye/wye.log`; or start one with `wye serve`; or name another with `--url` or `WYE_URL` |
| `cannot listen on ...: the port is in use` | Another gateway, perhaps | Stop it, or change `server.listen` and set `WYE_URL` |
| `401 invalid or missing gateway token` | One message for every token failure, on purpose | `wye explain --token-file <file> request.json` says which: expired, wrong audience, unknown key. Any other `401` is the provider's |
| `403 permission_error` | The token may not use that model, or asks for too many output tokens | `wye explain`; [permissions](policy.md#gateway-tokens) |
| `404 no route serves ...` | No route for that API | Add the section it names, such as `[routes.openai]` |
| `408 request body stalled` | While Wye read `model`, the client sent nothing for 60 s, or had not reached it after 5 minutes | Check the client's network |
| `415 the request body must...` | A compressed or non-UTF-8 body under a policy that reads it (permissions, ceilings, aliases, Bedrock) | Send plain UTF-8 JSON |
| `529 overloaded_error` (`503` on the OpenAI APIs) | From Wye, says `gateway is...`: past `max_concurrent_requests` or `max_held_request_bytes`. Otherwise from the provider | Clients retry on their own; raise the limit if it persists |
| One `429` at each session start | Claude Code's probe on a claude.ai login; Anthropic answers it with or without Wye | Nothing |

[Failure behavior](operations.md#failure-behavior) lists the rest.

## FAQ

### Does it change my requests?

Only the `model` value, when config maps an alias or the provider has its own id (Bedrock). Other body bytes are forwarded as sent. Headers change as a proxy's must: a gateway holding a key replaces the caller's credential with it, hop-by-hop headers go, and `accept-encoding` becomes `identity` so usage can be read. A request Wye cannot allow is refused, never altered ([forwarding contract](architecture.md#forwarding-contract)).

### Does it see my prompts?

They pass through the process, and it reads only what it needs: `model` and the output-token field under a policy, a few structural fields with `[upgrade_check]`, keyed hashes of prompt sections with `[cache_diagnostics]`, and usage, model, and tool names in responses. Records never contain prompts, completions, or credentials ([telemetry](operations.md#telemetry)).

### What does it store?

Nothing between requests: no database and no state shared between replicas. Its only files are the ones you give it: the config, keys, and the logs the service appends to ([files](operations.md#files)). Usage goes where you send stdout and, with `[telemetry]`, OTLP.

### Why not a config in the working directory?

A config decides where your credentials go, so one in a repository you just cloned must not apply by itself. Name it: `-c wye.toml`, or `export WYE_CONFIG=$PWD/wye.toml`.

### Does my subscription work?

Yes, for Claude Code on a claude.ai plan (Pro or Max), with the default `wye init`. It counts against your plan, on this machine only: the mode refuses to listen beyond loopback and cannot combine with gateway tokens. Unset `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN`, either of which replaces your login. `wye codex` uses an OpenAI key.

### Why is there no cost column?

Nothing carries a cost until the config has [`[pricing]`](policy.md#pricing-metadata), and `wye init` writes none: a table in the binary would go stale the day a provider changed its prices. Add one and restart. Records already written stay unpriced, and the report marks its total with `+`.
