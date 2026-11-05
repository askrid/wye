# Operations

Distribution, CLI, deployment, usage records and telemetry, and failure behavior. Wye is an ordinary process: IaC, DNS, certificates, infrastructure secrets, scheduling, and autoscaling are the deployment's.

## Distribution

- Release archives: a `v*` tag on a commit that passed `ci` on `main` publishes a GitHub Release with `.tar.xz` archives for x86_64 and arm64 Linux (glibc 2.35 or newer) and macOS, SHA-256 checksums, build provenance attestations (`gh attestation verify <archive> --repo askrid/wye`), and a shell installer. No Windows build. How the release is built: [development.md](development.md#continuous-integration).
- An OCI image, built locally only. The `Dockerfile` builds a release binary into distroless `cc` (glibc, no shell, non-root), with a `HEALTHCHECK` that runs `wye probe` against `/readyz`. Inside a container Wye listens on `0.0.0.0`, which requires `[auth]`. `scripts/image-smoke.sh` checks that the image becomes healthy, answers `/readyz`, and refuses an unauthenticated request.
- Planned: a published image, a Docker Compose example, and a Helm chart ([roadmap](roadmap.md#m1-team-ready-claude-code)).

## CLI

`wye --help` lists the commands and `wye <command> --help` a command's options. Both are generated from the table the parser reads, so help cannot drift. Options follow the GNU conventions: any order, `--name value` or `--name=value`, `-c` for `--config`, and `--` before arguments that start with a dash.

**Exit status**: 0, or 1 with an error on stderr; 3 when `doctor` finds a problem, `explain` a refusal, `policy-test` a failed case, `policy-diff` widened access, or `service status` a service that is not installed or does not answer.

**Output.** Help and `wye report` fit 80 columns; `explain` prints one long line per step, for `grep`. Data goes to stdout and messages to stderr, and a reader going away (`wye report | head`) ends a command quietly. On a terminal, color marks only what carries meaning: bold headings, cyan for a command to copy, and green or red for verdicts and errors; never elsewhere.

### Files

A user's files live in `~/.wye`, or `$WYE_HOME`:

| File | What it is | Written by | Read by |
|---|---|---|---|
| `wye.toml` | The config | `wye init` | every command that takes `-c` |
| `policy-tests/` | Sample requests and the decisions they expect, next to the config | `wye init` | `wye policy-test`, `wye explain` |
| `usage.jsonl` | The usage log: usage records | `wye service`, or your redirect | `wye report` |
| `wye.log` | The message log: the background gateway's messages | `wye service` | you, when it does not answer |
| `signing.key` | The signing key, mode 600 | `wye init --shared`, `wye token keygen` | `wye token issue`; never the gateway |
| `anthropic.key`, `openai.key` | Provider keys, next to a config that names them | you | the gateway |

A command that reads a config takes `-c <path>`, else `$WYE_CONFIG`, else `~/.wye/wye.toml`. A `wye.toml` in the working directory is read only when named (`-c wye.toml`, or `export WYE_CONFIG=$PWD/wye.toml` in a config repository): a config decides where your credentials go, so the one in a repository you just cloned must not apply by itself. `serve`, `doctor`, `explain`, `policy-test`, and `validate` print the config they read, and errors name it.

### Environment variables

Read by `wye`:

| Variable | Meaning | Used by |
|---|---|---|
| `WYE_CONFIG` | The config, when `-c` is not given; the container image sets `/etc/wye/wye.toml` | every command that takes `-c` |
| `WYE_HOME` | Your files (default `~/.wye`) | every command |
| `WYE_URL` | The gateway (default `http://127.0.0.1:8080`) | `claude`, `codex`, `probe` |
| `WYE_TOKEN` | The gateway token; removed from the agent's environment | `claude`, `codex` |
| `WYE_CLAUDE_BIN`, `WYE_CODEX_BIN` | Another path to the agent | `claude`, `codex` |
| `NO_COLOR`, `TERM` | No color when `NO_COLOR` is set or `TERM=dumb` | every command |

Set but empty counts as unset, except `WYE_TOKEN`, where it is an error instead of leaving the agent on its own login.

Named by the config, read once at startup; the names are yours to choose, and these are the usual ones:

| Field | Usual variable | Holds |
|---|---|---|
| `provider_key_env` | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` | A provider key |
| `cache_diagnostics.hash_key_env` | `WYE_CACHE_HASH_KEY` | The key for prompt-section hashes |
| `telemetry.otlp_headers_env` | | Headers for OTLP export |

Set for the agent by `wye claude` and `wye codex`:

| Variable | Set to |
|---|---|
| `ANTHROPIC_BASE_URL` | The gateway |
| `ANTHROPIC_AUTH_TOKEN` | The gateway token, if there is one |
| `CLAUDE_CODE_GATEWAY_HINT_HEADERS` | `1`, unless you set it |
| `WYE_CODEX_TOKEN` | The token Codex sends |

With a gateway token, `ANTHROPIC_API_KEY` is removed, since Claude Code would prefer it.

Clients read their own: the SDKs' `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`, `OPENAI_BASE_URL`, and `OPENAI_API_KEY` ([using it from apps](guide.md#using-it-from-apps)), and Claude Code's `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` and `CLAUDE_CODE_API_KEY_HELPER_TTL_MS`. Variables for the benchmark and scripts are in [development.md](development.md#scripts-and-benchmark-variables).

### Commands

What `--help` does not say.

Run with no argument, every command does what its name says on the config it finds, or fails with one line that says what to run next: `wye explain` names a sample from `policy-tests/`, `wye claude` and `wye probe` say to run `wye init` when nothing answers and there is no config, and `wye token keygen` refuses to replace the key `wye token issue` signs with.

**`wye init`**: [choosing a setup](guide.md#choosing-a-setup). Next to the config it writes `policy-tests/`: a Messages request that forwards (with `--shared`, from a caller with a gateway token), a Responses request with `--openai`, and with `--shared` a request without a gateway token, refused with `401`. They pass against the config it wrote, so `wye policy-test` passes right after `wye init`, and fails once a config change alters one of those decisions. A case whose request or expectation exists is kept as it is. With `--shared`, the signing key goes to `~/.wye/signing.key`, and the next steps say to keep it off the hosts that serve: it issues gateway tokens, and `wye serve` never reads it.

**`wye doctor`** checks what `serve` will need and says what to change, one line per check. Secrets go through the same functions `serve` resolves them with, so a check fails exactly when startup would. Every check runs, so one pass lists every problem.

- Each upstream's provider key is present, not blank, and valid in an HTTP header (never printed), and how it will be sent.
- A `provider_key_file`, and the signing key `wye token issue` uses by default (`~/.wye/signing.key`) when this machine has one, is closed to group and others; otherwise the check fails with the `chmod 600` to run. `serve` warns of an open `provider_key_file` in the message log, and starts. A Kubernetes secret volume is mounted `0644` unless its `defaultMode` says otherwise.
- A Bedrock upstream's base URL ends in `/anthropic`, where its Messages endpoint is.
- Each upstream answers at its base URL: one unauthenticated `HEAD`, trusting its `ca_file`, proves DNS, TCP, TLS, and HTTP, and nothing is billed.
- The identity providers' key sets, the cache-diagnostics hash key's length, and the OTLP header variable and endpoint. (Config refuses an OTLP endpoint over plain http beyond this machine, as it refuses such a base URL: its headers usually carry an OpenTelemetry backend's API key.)

Not checked yet: model availability per upstream.

**`wye validate`** parses the config and reads the identity providers' key sets, without starting the gateway or resolving secrets, so CI can run it without credentials. `wye doctor` checks the rest.

**`wye explain`** prints each step `serve` would take for one request, offline, calling the same functions in the same order: endpoint (`--endpoint`, default `/v1/messages`), gateway token (verified at the current time; `--token-file`, never `$WYE_TOKEN`), or without `[auth]` whether a web page may have sent it ([policy.md](policy.md#a-gateway-without-auth); `--header 'Origin: ...'`, and by default a local client's `Host`, the listen address), body size, alias, model permission, output-token ceiling, [upstream selection](glossary.md#requests-and-where-they-go) and failover order, upstream URL and the credential (a provider key by name, never value, or the caller's own under passthrough), the provider's model id, the price, whether cache diagnostics would hash it, and, with `--target-model` or `[upgrade_check]`, what each target would reject, the fix, and the source.

A refusal prints the status and error the gateway would send. The body is read as the gateway reads it: up to its `model`, then the rest when permissions or a ceiling apply. Not evaluated: run-time conditions (the concurrency and connection limits, the held-bytes budget, upstream failures) and request headers beyond `Host`, `Origin`, `Sec-Fetch-Site`, and the session id (the gateway answers a compressed or non-UTF-8 body under a policy with `415`).

**`wye policy-test [<dir>]`**: `<name>.expect.toml` states the decision for the request body `<name>.json`: `decision = "forward"` or `"refuse"`, optionally `status`, `error`, `model` (the [served model](glossary.md#models)), `upstream`, `endpoint` (default `/v1/messages`), `session`, and a `[token]` table (`subject`, `models`, `max_output_tokens`). Tokens would expire in CI, so a case states the token's claims and they are taken as verified: signature, issuer, audience, key, and expiry are not checked. Without `<dir>`, the cases are in `policy-tests/` next to the config, where `wye init` writes samples. [`examples/team/`](../examples/team) and [`examples/app/`](../examples/app) each have a config and its cases, and the test suite runs them.

**`wye policy-diff <old> <new>`** prints what a config change widens, narrows, or only changes, and exits 3 when anything widens, so CI can ask for a second review. Key sets are read relative to each config, so compare against a checkout of the base: `git worktree add ../base main && wye policy-diff ../base/wye.toml wye.toml`.

- Widening: authentication removed, a listener moved beyond loopback or onto every interface, a model newly allowed by `[auth] models` or an identity provider's grant, a raised or removed `max_output_tokens`, a longer `max_gateway_token_ttl_secs`, a newly trusted [issuer](glossary.md#access) (a signing key, Wye tokens, an identity provider, a key in its key set, or a changed audience, algorithm, or subject claim), a route for an API that had none, and an upstream that starts sending a provider key where callers brought their own.
- Model patterns compare by coverage, so `claude-*` → `claude-opus-*` narrows; output-token ceilings and trusted issuers compare by value.
- Listed for review only: upstreams added, removed, or moved within a route, and a changed provider-key source. Where requests go is not who may send them.
- Not compared: aliases (permissions apply to the served model) and tokens (a token can only narrow what config allows).

**`wye report`**: [reading it](guide.md#reading-wye-report).

**`wye probe [<url>]`** sends one `GET` and exits 0 only for a 2xx answer within 5 s. A URL without a path names a gateway and asks its `/healthz` (the gateway's `/` serves nothing). A path is asked as given, such as `/readyz` for a load balancer's view.

**`wye token issue <subject>`** signs a gateway token offline ([policy.md](policy.md#gateway-tokens)) and never runs inside the gateway. The key id comes from the `[[auth.signing_keys]]` entry whose public half matches the key, so a rotated key needs no flag. A `--ttl` above `max_gateway_token_ttl_secs` is refused, as the gateway would refuse the token. A service that mints tokens does the same with a config of its own.

### Agent wrapper

**`wye claude`** checks that the gateway answers `/healthz`, sets the [variables above](#environment-variables), and replaces itself with `claude` (Unix `exec`). Signals, the terminal, and the exit code are Claude Code's own, and nothing is written. Without a gateway token, Claude Code keeps its own login or API key.

- `claude` is found on `PATH`; a shell alias is not applied.
- With a gateway token, Claude Code gets it as `ANTHROPIC_AUTH_TOKEN`, which every tool and hook Claude Code runs inherits: a command it runs can read the token and use it until it expires. To keep the token out of the environment, have Claude Code fetch it with [`apiKeyHelper`](#token-renewal) instead, and run `wye claude` without `--token-file` or `WYE_TOKEN`.
- Arguments after `--`, or from the first one wye does not know, go to Claude Code unchanged (`wye claude --resume <id>`; `wye claude -- --help` for its own help).
- A gateway beyond this machine must be `https`: plain http would carry the gateway token, or Claude Code's own login, in the clear.
- Claude Code sends hint headers (request class, prompt id, tool times) to a custom base URL only when asked, hence `CLAUDE_CODE_GATEWAY_HINT_HEADERS`.
- The URL and the hint setting also go to Claude Code as `--settings`, because background sessions (`wye claude --bg "task"`) are started by its daemon, which does not inherit the wrapper's environment. The gateway token never goes that way, since other local users can read a command line; a background session behind `[auth]` gets its token from [`apiKeyHelper`](#token-renewal).
- `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` makes Claude Code's `/model` picker read `GET /v1/models` when the config has `[discovery]`.

**`wye codex`** takes the same options and environment. It runs `codex` with command-line config overrides that define a provider `wye` (`base_url` = the gateway plus `/v1`, `wire_api = "responses"`, `env_key = "WYE_CODEX_TOKEN"`) and select it. Without a gateway token, `WYE_CODEX_TOKEN` is a placeholder, which a gateway without `[auth]` replaces with its OpenAI provider key. `~/.codex/config.toml` is not touched; arguments after wye's own go to Codex (`wye codex exec "fix the tests"`). The gateway needs `[routes.openai]`; with `[auth]`, use a gateway token without an [output-token ceiling](policy.md#per-request-ceilings). What Codex sends: [architecture.md](architecture.md#codex-integration).

### Token renewal

Short-lived tokens need a command that fetches a fresh one, so nobody pastes tokens. Claude Code's [`apiKeyHelper`](https://code.claude.com/docs/en/llm-gateway-connect) setting runs a shell command that prints the token and sends its output as both `Authorization: Bearer` and `x-api-key`. It caches the token for five minutes (`CLAUDE_CODE_API_KEY_HELPER_TTL_MS` changes that).

`ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` take precedence over the helper, so leave both unset (and run `wye claude` without `--token-file` or `WYE_TOKEN`). The helper can be distributed through managed settings. Each new token takes one full verification per connection, then the [per-connection cache](policy.md#gateway-tokens) applies.

In GitHub Actions, the job's [identity-provider token](glossary.md#access) (GitHub's OIDC token) is the gateway token, so the repository holds no secret. Wye trusts the issuer ([`[[auth.identity_providers]]`](policy.md#gateway-tokens)):

```toml
[[auth.identity_providers]]
issuer = "https://token.actions.githubusercontent.com"
audience = "wye-prod"
algorithm = "RS256"
# from https://token.actions.githubusercontent.com/.well-known/jwks
jwks_file = "github.jwks"
subject_claim = "repository"       # usage per repository
[[auth.identity_providers.grants]]
claim = "repository_owner"
value = "acme"
models = ["claude-sonnet-*", "claude-haiku-*"]
```

The job asks GitHub for a token with Wye's audience on every helper run:

```sh
#!/bin/sh
# scripts/wye-token.sh: prints a fresh GitHub Actions OIDC token for Wye.
curl -fsS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
  "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=wye-prod" | jq -r .value
```

```yaml
permissions:
  id-token: write                  # lets the job request OIDC tokens
  contents: read
steps:
  - uses: actions/checkout@v7
  - run: |
      mkdir -p .claude
      echo '{"apiKeyHelper": "sh scripts/wye-token.sh"}' > .claude/settings.local.json
      claude -p "fix the failing tests"
    env:
      ANTHROPIC_BASE_URL: https://wye.internal.example.com
      CLAUDE_CODE_API_KEY_HELPER_TTL_MS: "240000"   # refresh within the token's lifetime
```

GitHub does not document the token lifetime (its example token is valid for five minutes), so keep the helper's cache lifetime below it; Wye refuses an expired token with `401`, and `max_gateway_token_ttl_secs` bounds what it accepts.

Developers use the same setting with a helper that prints a token from an issuer Wye trusts: the team's IdP (an `[[auth.identity_providers]]` entry), or an internal service that authenticates the developer by SSO and mints a short-lived gateway token with `wye token issue` (the signing key stays in that service). Offboarding then happens at the IdP: the next renewal fails, and the last token expires on its own ([revocation](policy.md#revocation)).

## Deployment

| Setup | Topology | Integration |
|---|---|---|
| This machine: passthrough or provider key (`wye init`, `wye init --provider-key`) | Native binary, as a user service | `wye service install`, then `wye claude` |
| Shared (`wye init --shared`): a team, or an app's services | Containers or VMs behind HTTPS | SDK base URL + gateway token |

A sidecar beside an app ([guide](guide.md#using-it-from-apps)) is the provider-key setup on loopback in the app's host or pod; it has not yet run in production. Other shapes, such as Kubernetes across regions or a serverless platform, are untested.

Wye itself speaks plain HTTP, so a gateway beyond one machine goes behind something that terminates TLS: a platform's load balancer, an ingress, or a proxy on the same host; `wye claude` and `wye codex` [refuse](#agent-wrapper) a remote gateway over plain http. With [Caddy](https://caddyserver.com/docs/quick-starts/reverse-proxy), which gets a certificate on its own and flushes event streams as they arrive (not yet run against Wye):

```text
wye.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Whatever sits in front must not buffer event streams (nginx: `proxy_buffering off`), and its idle timeout must exceed silent gaps in a stream (around 300 s; the AWS ALB default of 60 s is too short). An app server that relays streams needs a server model suited to long-lived streams (ASGI rather than blocking WSGI workers).

### Running as a service

On one machine, `wye service install` keeps the gateway running across logins and restarts it if it stops (on Linux, if it fails), through the operating system's own service manager, as you. It needs no sudo and changes nothing outside your home directory.

It writes a LaunchAgent (`~/Library/LaunchAgents/dev.wye.gateway.plist`) on macOS or a systemd user unit (`~/.config/systemd/user/wye.service`) on Linux, which runs `wye serve --config <config>` with stdout appended to the usage log `usage.jsonl` and stderr to the message log `wye.log`. `--dry-run` prints the plan and the file and changes nothing. Before it changes anything it shows what it will do and asks; `--yes` skips the question (required without a terminal).

Before installing, the config is checked as the service will start it: it parses, every file it names is readable, and no secret comes from an environment variable, since a service does not inherit your shell's environment.

- Provider keys go in files next to the config (`provider_key_file = "anthropic.key"`, readable only by you), which is how `wye init` writes them.
- The cache-diagnostics hash key and OTLP headers come only from the environment for now, so a config that uses them runs `wye serve` under a service definition of your own. Start from what `--dry-run` prints for a config without them.
- If another gateway already answers on the config's port, such as `wye serve` in a terminal, the install stops.
- Under systemd, it refuses paths containing spaces, quotes, `$`, or a backslash.

```bash
wye service status      # installed? what it runs, its logs, whether it answers
wye service restart     # after a config edit or a new wye
wye service uninstall   # stops it and removes the file; the config, the usage log, and the message log stay
```

`restart` refuses a config that would not start and leaves the running service as it was; when this `wye` or the config's drain and grace settings call for a new service definition, it writes one and has the service manager load it. For another config, or a `wye` at another path, run `wye service install` again (`-c <path>`); it replaces the definition.

Replacing a running service waits for its gateway to finish its requests, up to the stop timeout: the config's drain delay and shutdown grace plus 5 s (35 s by default). New connections are refused until then. Open streams end after that grace; Claude Code retries them.

The definition sets the service's open-file limit to 65536, soft and hard (launchd's `SoftResourceLimits` and `HardResourceLimits`, systemd's `LimitNOFILE`); launchd would start it with 256 and systemd with 1024. [`server.max_connections`](policy.md#local-resource-bounds) needs about twice its value, a client and an upstream connection each: 40,000 at the default 20,000. `wye serve` run by hand or in a container keeps the limit it inherits: the shell's (`ulimit -n`, often 256 on macOS and 1024 on Linux; raise it with `ulimit -n 65536`) or the container runtime's (Docker's default is far higher; Kubernetes inherits the node runtime's). A definition of your own sets it the same way.

On Linux, a user service stops when you log out unless lingering is on (`loginctl enable-linger`). The usage log grows without limit: rotate it, or truncate it after `wye report`.

### Without Kubernetes

Because Wye keeps no state and has no peer discovery, every multi-node deployment has the same shape: a load balancer in front of identical processes reading the same config, with its health check on [`/readyz`](#health-and-shutdown). An Auto Scaling group, ECS, Docker Swarm, Nomad, or VMs behind HAProxy all fit; none has run Wye yet.

For container platforms, `provider_key_file` reads the provider key from a mounted secret file at startup, and [`wye probe`](#commands) is the health check for images without `curl`. Several upstreams behind one route: [guide](guide.md#several-upstreams).

### Sizing (estimate)

From the TLS-path benchmark on a laptop, not yet measured on EC2: Wye's [overhead](glossary.md#usage-and-cost) for a heavy Claude Code user (about 5 concurrent streams, 250 events/s, 150 KB/s of resent context) is about 10 ms of CPU per second, about 1% of a core, and about 250 KB of memory (49 KB of heap per stream after a large upload, from the allocation benchmark). A 2-vCPU instance would serve roughly 130 heavy or 400 typical users at 70% CPU. Provider rate limits bind first; run two small instances for availability rather than capacity. Confirm with `wye-bench` on the target instance type and a separate load machine.

Wye does not lower a stream's tokens per second. It adds a roughly constant delay per event that does not accumulate ([measured](benchmarks.md#current-harness-and-baseline)), small against the provider's 10-20 ms between tokens. Model permissions and cache diagnostics add overhead per request and per uploaded MB, not per streamed event.

One Tokio worker held about 1,500 paced streams (36,000 events/s) within 10 ms of added p99. Latency, not CPU, sets the limit, so size by streams per core ([measurements](benchmarks.md#current-harness-and-baseline)).

## Usage records and telemetry

One usage record per request, written as a JSON line to the usage log (stdout) and, with `[telemetry] otlp_endpoint`, exported as OTLP over HTTP with JSON encoding: a log record and a span per request, plus cumulative metrics. No Prometheus endpoint; a collector is optional. Records never contain prompts, completions, or credentials. Records and metrics are not a quota ledger: records can be lost and counters reset ([policy.md](policy.md#state-boundary)).

The usage record, field by field:

**Request and outcome.**

- `event`, `unix_ms`, `path`, `status`: `request`, time, endpoint, HTTP status.
- `outcome`: recorded when the response body is dropped, so every outcome is counted. An upstream's answer, error statuses included, is `completed`, `cancelled`, or `truncated`; Wye's own answer is `refused`, `upstream_error`, or `gateway_error`:
  - `completed`: the upstream's response body ended.
  - `cancelled`: the client left first; one that leaves before the response headers has `status` `499`.
  - `truncated`: the upstream's response broke after its headers were sent, by the idle timeout or a reset.
  - `refused`: Wye's [refusal](glossary.md#decisions) , its overload included; no upstream response.
  - `upstream_error`: no usable upstream response ([upstream error](glossary.md#requests-and-where-they-go)).
  - `gateway_error`: Wye's own fault, such as the `500` for an alias it could not apply.
- `upstream_error`, on `upstream_error` outcomes only: `connect` (no connection or TLS handshake, `502`), `timeout` (no response headers within `response_header_timeout_secs`, `504`), `reset` (the request failed after connecting, before the response headers, `502`).
- `duration_ms`, `response_headers_ms`: milliseconds, in all and to the response headers.
- `upstream`: the upstream's `id`; absent for a route's only upstream written without one, and for requests refused before upstream selection.
- `failovers`, such as `[{"upstream": "a", "reason": "429"}]`: attempts rejected before the one that answered, in order, each with its [failover reason](glossary.md#requests-and-where-they-go) ([failover](architecture.md#failover)); absent when none.
- `subject`: the gateway token's subject, with `[auth]`.
- `session_id`: the client's session (Claude Code's or Codex's), kept only as a bounded string (below).

**Model, usage, and cost.**

- `requested_model`: the [alias](architecture.md#model-aliases-and-upgrade-checks) the client named, only when an alias was resolved.
- `model`, `usage_status`: the [served model](glossary.md#models); `complete` | `partial` | `unknown` | `none`. Streaming and non-streaming (JSON) responses are read; `none` means the response carries no usage: errors, `count_tokens`, or other content types.
- `upgrade_issues`: target model → rules it would reject, for checked requests only ([upgrade checks](architecture.md#model-aliases-and-upgrade-checks)); `{}` when none.
- `cost_usd`, `cost_status`, `pricing_version`: an exact decimal string; `complete` | `partial` | `unknown`; `pricing.version`. With [`[pricing]`](policy.md#price-table); `cost_usd` only when a cost was computed.
- `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`: counts, as the provider reports them, except that on the OpenAI APIs the cached share of `input_tokens` (Chat: `prompt_tokens`) is moved to `cache_read_input_tokens`, so `input_tokens` excludes cache reads on every API.

**Agent.**

- `request_class` (`main` | `subagent` | `workflow` | `compaction` | `auxiliary` | `other`): Claude Code's hint header; for Codex, from `x-openai-subagent` ([mapping](architecture.md#codex-integration)).
- `agent_type`, `agent_id`, `parent_agent_id`, `prompt_id`: bounded strings. Claude Code: `agent_id`s on every subagent request; the rest need `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`, which `wye claude` sets. Codex: thread ids as agent ids, the subagent kind as `agent_type`; no prompt id.
- `compaction`, `after_compaction` (`auto` | `manual` | `reactive` | `other`): hint headers, on the compaction request and on the first main request after it.
- `tool_durations`, such as `[{"name": "Bash", "ms": 742}]`: tool calls since the agent's previous request; at most 32.
- `tool_calls`, such as `{"Bash": 2, "Read": 1}`: tool calls the response started (`tool_use` blocks, Responses call items, Chat tool calls); at most 8 names, the rest under `(other)`; inputs never read.

**Cache.**

- `cache_miss` (`compaction` | `after_compaction` | `unexplained`): a cache write with no cache read; the first cache write of a session or a subagent also reads `unexplained`.
- `prompt_hashes`: keyed prompt hashes, with [`[cache_diagnostics]`](architecture.md#cache-diagnostics), sampled sessions only: `status`, `tools` and `system` (`count`, `hash`), `messages`, `breakpoint_count`, up to 4 `breakpoints`, the last 8 `message_prefixes`; 16-hex HMAC-SHA256 tags.

Client-supplied values are labels, never authorization: closed vocabularies map to fixed strings, and ids outside 1-128 bytes of letters, digits, `-`, `_`, `.`, `:` are dropped. No `provider`: the `upstream` id names the upstream, and config maps it to its provider.

OTLP logs: each record becomes a log record at `{otlp_endpoint}/v1/logs`, resource `service.name` (`telemetry.service_name`, default `wye`) and `service.version`. Its attributes:

- OpenTelemetry semantic conventions where they exist: `gen_ai.provider.name`, `gen_ai.request.model` (`requested_model`), `gen_ai.response.model`, `gen_ai.usage.*_tokens`, `http.route`, `http.response.status_code`, `session.id`, `enduser.id`.
- `gen_ai.provider.name` is the provider of the upstream that served the request: `anthropic` (also for `claude-platform-aws`), `aws.bedrock`, or `openai`; for a request refused before an upstream was chosen, the endpoint's (`anthropic` or `openai`).
- Then the agent fields of either client: `gen_ai.agent.id`, `gen_ai.agent.name` for the agent type, `wye.parent_agent_id`, `wye.request_class`, `wye.prompt_id`, `wye.compaction`, `wye.after_compaction`.
- `wye.*` for the rest: `wye.outcome`, `wye.upstream_error`, `wye.duration_ms`, `wye.response_headers_ms`, `wye.usage_status`, `wye.upstream`, `wye.cost_usd`, `wye.cost_status`, `wye.pricing_version`, `wye.cache_miss`; `wye.tool_calls`, `wye.failovers`, `wye.prompt_hashes`, and `wye.upgrade_issues` as JSON strings.

OTLP metrics are cumulative since process start, exported every 10 s and at shutdown; at most 256 attribute sets each; per process, summed by the OpenTelemetry backend.

| Metric | Kind | Attributes |
|---|---|---|
| `wye.requests` | sum | `http.route`, `wye.upstream`, `http.response.status_code`, `wye.outcome` |
| `wye.request.duration` | histogram, ms (5 ms to 300 s) | `http.route`, `wye.outcome` |
| `wye.tokens` | sum | `gen_ai.token.type`, `gen_ai.response.model`, `wye.request_class`, `wye.upstream` |
| `wye.cost` | sum, USD | `gen_ai.response.model`, `wye.request_class`, `wye.upstream` |
| `wye.upgrade.issues` | sum | `wye.upgrade.target`, `wye.upgrade.rule`, `enduser.id` |
| `wye.failovers` | sum | `wye.failover.upstream`, `wye.failover.reason` |
| `wye.tool.calls` | sum | `gen_ai.tool.name` |
| `wye.cache.misses` | sum | `wye.cache_miss` |
| `wye.requests.active` | gauge | |
| `wye.records.dropped` | sum | |

- `wye.cost` counts complete and partial costs, with `[pricing]`.
- In `wye.requests`, `wye.outcome` takes the record's `outcome` values.
- `wye.upgrade.issues` counts [upgrade issues](glossary.md#models): one per target model and rule a checked request breaks.
- `wye.failovers` counts rejected attempts: `wye.failover.upstream` is the upstream that rejected one, and `wye.failover.reason` a status or `connect`.
- `wye.records.dropped` counts records the usage log dropped, OTLP log records and spans dropped by full queues or failed exports (a request losing both counts twice), and records past the series cap.
- `wye.upstream`, in `wye.requests`, `wye.tokens`, and `wye.cost`, is the upstream that answered; present only with a route's named upstreams (`[routes.claude]`, `[routes.openai]`).

OTLP traces have one server span per request (`POST` and the endpoint, such as `POST /v1/messages`) with the record's attributes, a `response headers` event, and `ERROR` status for the outcomes `upstream_error`, `gateway_error`, and `truncated` and for an upstream's 5xx; a `refused` request, Wye's overload included, leaves the status unset, as OpenTelemetry does for a server's 4xx. Its parent is the first that applies:

1. A valid `traceparent` header.
2. With a client session id (Claude Code's or Codex's), the session's trace: trace id = first 16 bytes of SHA-256 of the prefixed session id, the same on every replica, so a whole session is one timeline.
3. A new trace.

Each reported tool call becomes a sibling span `execute_tool <name>` (`gen_ai.operation.name` `execute_tool`, `gen_ai.tool.name`, `wye.tool.duration_ms`) ending when the request starts (approximate: Claude Code's times exclude prompts and hooks). No synthetic prompt or subagent spans: no single request knows it is the first. The `traceparent` sent upstream is the caller's.

Bounds: records pass a 4,096-record queue to one writer task; OTLP batches go at 512 records or after 1 s through a queue of 8 batches, with a 10 s timeout per export. Anything dropped is counted and written to the usage log at shutdown (`usage_log_dropped` and `otlp_dropped` events, each with its `records` count). A blocked usage log keeps OTLP running: up to 4,096 records, as many as the queue holds, are kept for the log's next write, and the rest are counted as dropped.

`telemetry.otlp_headers_env` names an environment variable of extra export headers (`key=value,key2=value2`), treated as secrets; the export client trusts the upstreams' roots, every `ca_file` included.

## Health and shutdown

`/healthz` says the process is up, and is what `wye probe`, `wye claude`, and `wye service` ask. `/readyz` also fails while draining, so it is the one for load balancers and container health checks. Neither depends on the OpenTelemetry backend: a transient outage there does not block inference.

On SIGTERM, shutdown has two phases:

1. Draining (`server.drain_delay_secs`, default 0): `/readyz` returns 503 while `/healthz` stays 200, and new connections and requests are still served, so a load balancer removes the replica before connections are refused. Set it to cover the load balancer's probe interval and endpoint propagation (in Kubernetes, typically 5-15 s) instead of a `preStop` sleep.
2. Grace (`server.shutdown_grace_secs`, default 30): the listener closes, active requests may finish, and whatever remains is cancelled; cancelling a stream closes its upstream request.

Give the platform a stop timeout (Kubernetes: `terminationGracePeriodSeconds`) above the sum of both.

## Failure behavior

- **Bad config or missing secret**: fail startup with an actionable diagnostic, without exposing secrets.
- **Invalid or expired gateway token**: refuse locally.
- **No eligible route**: explicit compatibility or policy error.
- **Slow downstream**: bounded flow control ([upstream connections](architecture.md#upstream-connections)): streams sharing its upstream connection are unaffected, and a client that accepts no bytes for `client_write_timeout_secs` (60) is disconnected and its upstream request cancelled.
- **Client body breaks** (bad framing, cut off mid-upload): `400`, recorded as `refused`; the upstream request is aborted and not failed over.
- **Client stops sending while its body is held**: `408` after 60 s without bytes, or 5 minutes without reaching its `model`; its held bytes and permit are released.
- **Request body breaks the policy or the size limit mid-upload**: the upstream request is aborted before the provider sees the body whole; the client gets the refusal's own `400`, `403`, or `413`, from HTTP/1.1 and HTTP/2 upstreams alike.
- **Upstream error or rejection before output**: `502` or `504`, recorded as `upstream_error` with its kind; with `failover` on the route, the session's next upstream when the rejection proves the request was not processed ([failover](architecture.md#failover)).
- **Partial stream failure**: terminate the stream; never silently replay output.
  - A successful Messages event stream that stalls past `stream_idle_timeout_secs` or breaks at an event boundary ends with an Anthropic `error` event (`overloaded_error`) and a clean close, so clients see a retryable API error.
  - A Responses stream in the same case just ends, which Codex and the OpenAI SDKs retry.
  - Mid-event failures still drop the stream, since appending would corrupt the event.
- **Client disconnect**: cancel upstream where possible, free resources, record uncertain cost.
- **Telemetry export failure**: inference continues; loss visible in logs/counters.
- **One replica dies**: others accept new connections; its streams fail.
- **Fleet restart**: redeploy config and secrets; in-flight inference lost, no quota state to restore.
- **Mixed config versions during rollout**: each request is decided by one replica's config, and records do not yet say which.
