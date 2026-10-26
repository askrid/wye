# Operations

Distribution, CLI, deployment, telemetry, and failure behavior. The gateway binary is an ordinary inference server: IaC, DNS, certificates, infrastructure secrets, scheduling, and autoscaling are not implemented inside it.

## Distribution

Only the OCI image is built so far; everything else in this list is planned.

- Versioned Linux and macOS binaries (x86-64, ARM64 as supported). Today: build from source ([local setup](#client-wrapper)).
- Signed, versioned OCI image; no consumer Dockerfile required. **Implemented locally:** the repository `Dockerfile` builds a release binary into distroless `cc` (glibc, no shell, non-root; about 12 MB), with a `HEALTHCHECK` that runs `wye probe` against `/readyz`. Inside a container Wye listens on `0.0.0.0`, which requires an `[auth]` section. `scripts/image-smoke.sh` builds the image and checks that it becomes healthy, answers `/readyz`, and rejects an unauthenticated request before any upstream contact. Publishing and signing are not set up.
- Docker Compose example for a VM.
- Helm chart for Kubernetes.
- Terraform reference examples where useful.
- Later: Cloudflare Workers / Fastly Compute Rust/WASM adapter, subject to streaming and API constraints.

## CLI

### Server

```bash
wye serve --config wye.toml
wye check-config wye.toml
wye explain --config wye.toml [--credential token.txt] [--session <id>] [--path <path>] [--target-model <model>]... request.json
wye token keygen --out issuer.key
wye token issue --key issuer.key --kid k1 --issuer wye --audience wye-prod --subject team-research --ttl-secs 3600
wye policy-test --config wye.toml policy-tests/
wye report usage.jsonl
wye probe http://127.0.0.1:8080/readyz
wye doctor
```

Implemented: `serve`, `check-config` (parses the config and reads the identity providers' key sets; environment secrets are not checked), `token`, `policy-test`, `report`, `probe`, and a first `explain`; `doctor` is planned.

`probe` sends one `GET` over HTTP or HTTPS and exits 0 only for a 2xx answer within 5 s: a health check for images without `curl` (Docker `HEALTHCHECK CMD ["wye", "probe", "http://127.0.0.1:8080/readyz"]`), and the check `wye claude` runs against a local or remote gateway.

`doctor` (planned) checks local config, resolved environment variables, DNS/TLS/provider connectivity, model availability, and compatibility warnings, without an admin API.

**Implemented: `wye explain`.** Prints each check `serve` would make for one request, calling the same functions in the same order, offline: endpoint, credential (verified at the current time), body size, alias resolution, model permission, `max_tokens` ceiling, the route upstream with `[routes.claude]` (and whether the session id or the credential subject chose it), upstream URL and provider credential (by name, never value), and whether cache diagnostics would hash it; with `--target-model` (or `[upgrade_check]`), what each target model would reject, the fix, and the source. Exit 0 for forward, 3 for a refusal, with the status and error the server would send. Run-time conditions (the concurrency limit, the held-bytes budget, upstream failures) are not evaluated. With failover and cost ceilings it will also print rejected candidates and why, and the cost bound. Routing is a pure function of request, credential, and config, so the answer is what every replica would do, and committed sample requests make policy testable in CI: a change that reroutes traffic or raises a cost bound fails review.

**Implemented: `wye policy-test --config wye.toml <dir>`.** Each `<name>.json` in the directory is a request body and `<name>.expect.toml` states the decision: `decision = "forward"` or `"refuse"`, optionally `status`, `error`, `model` (the model the request runs on, after aliases), `upstream` (the `[routes.claude]` upstream it reaches), `path`, `session`, and a `[credential]` table (`subject`, `models`, `max_output_tokens`). Tokens would expire in CI, so a case states the credential's claims and only the signature check is skipped; every later check is the server's, through `explain`. It prints one line per case and exits 3 if any fails. [`examples/policy-tests/`](../examples/policy-tests) has a config and four cases (the background Haiku model allowed, Opus above the configured ceiling refused, no credential refused, an alias resolved by a canary rollout rule), and the test suite runs them.

```text
policy-tests/
├── background-haiku.json          # request
├── background-haiku.expect.toml   # decision = "forward", [credential] models = [...]
├── opus-beyond-ceiling.json
└── opus-beyond-ceiling.expect.toml  # decision = "refuse", status = 403
```

With cost ceilings, expectations will also bound a cost (`max_cost_usd <= "0.05"`).

**Implemented: `wye report [<usage.jsonl>]`.** Summarizes a usage log (the JSON lines `wye serve` writes to stderr; standard input by default) with no telemetry backend: requests and outcomes; tokens, cache-hit rate (cache reads as a share of prompt tokens), cost (with `[pricing]`: summed exactly from `cost_usd`, `+` when it includes partial or unknown costs), request time, and tool time by request class; cache misses split into unexplained ones, session starts, compactions, and the first request after a compaction (a session's first request is an expected miss that one request cannot recognize, but the whole log can); tool calls the model requested, and calls and time Claude Code reported, by tool; requests, tokens, and cost by upstream, when records name one ([routes](architecture.md#cache-affine-routing)); upgrade issues by target, rule, and client; and the ten sessions and prompts with the most tokens. Other lines are skipped and counted. `wye serve --config wye.toml 2>usage.jsonl` keeps the log; `wye report usage.jsonl` reads it.

`token issue` signs gateway credentials offline from a private key file ([policy.md](policy.md#gateway-credentials)). It never runs inside the server. `--models <name,prefix*,...>` limits the credential to those models, and `--max-output-tokens <n>` lowers its `max_tokens` ceiling.

### Client wrapper

**Implemented: `wye claude`.** `wye claude [--gateway <url>] [--credential-file <path>] [--] [claude args...]` checks that the gateway answers `/healthz`, sets `ANTHROPIC_BASE_URL` (default `http://127.0.0.1:8080`, or `WYE_GATEWAY_URL`) and, with a gateway credential (`--credential-file` or `WYE_CREDENTIAL`), `ANTHROPIC_AUTH_TOKEN` while removing `ANTHROPIC_API_KEY`, and then replaces itself with `claude` (Unix `exec`): signals, the terminal, and the exit code are Claude Code's own, and nothing is written, so nothing needs cleaning up. Without a credential, Claude Code keeps its own login (subscription passthrough). `claude` is found on `PATH` (a shell alias is not applied; `WYE_CLAUDE_BIN` names another program), and arguments after `--` go to Claude Code unchanged.

**Local setup (macOS or Linux, from source).** Install the binary with `cargo install --path crates/wye --locked` (into `~/.cargo/bin`; rerun after pulling changes). Then run the gateway in one terminal and Claude Code in another:

```bash
mkdir -p ~/.wye
wye serve --config examples/subscription.toml 2>>~/.wye/usage.jsonl   # claude.ai subscription
# or: ANTHROPIC_API_KEY=... wye serve --config examples/wye.toml       # API key

unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN   # subscription: keep Claude Code's own login
export CLAUDE_CODE_GATEWAY_HINT_HEADERS=1      # request class, prompt id, tool durations
wye claude                                     # wye claude -- --resume, etc.

wye report ~/.wye/usage.jsonl                  # afterwards, or at any time
```

The usage log goes to the gateway's stderr, so redirecting it keeps it; `wye report` skips the startup lines. `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` makes Claude Code's `/model` picker read `GET /v1/models` when the config has `[discovery]`.

**Implemented: `wye codex`.** Same flags and environment as `wye claude`. It runs `codex` (or `WYE_CODEX_BIN`) with command-line config overrides that define a provider `wye` (`base_url` = the gateway plus `/v1`, `wire_api = "responses"`, `env_key = "WYE_CODEX_TOKEN"`) and select it, and sets `WYE_CODEX_TOKEN` to the gateway credential (a placeholder without one, which a gateway without `[auth]` replaces with its OpenAI key). `~/.codex/config.toml` is not touched; arguments after the flags go to Codex (`wye codex exec "fix the tests"`). The gateway needs `[routes.openai]`; with `[auth]`, use a credential without an output-token ceiling, since Codex does not set `max_output_tokens` ([policy.md](policy.md#gateway-credentials)). What Codex sends: [architecture.md](architecture.md#codex-integration).

Proposals, not built: `wye codex` (with M3), and profiles that select a gateway and credential by name.

### SDK integration

Apps and custom agents keep the provider's official SDK and point it at Wye; the gateway credential takes the place of the provider key (Wye reads it from `x-api-key` or `Authorization: Bearer`):

```python
from anthropic import Anthropic

client = Anthropic(
    base_url="https://gateway.example.com",
    api_key="gateway-credential",
)
```

The OpenAI SDK connects the same way to `/v1/responses` when the config has `[routes.openai]` (`base_url="https://gateway.example.com/v1"`, the gateway credential as `api_key`). Claude Code connects through environment variables:

```bash
export ANTHROPIC_BASE_URL=https://gateway.example.com
export ANTHROPIC_AUTH_TOKEN=gateway-credential   # or apiKeyHelper for automatic renewal
```

### Credential renewal

Short-lived credentials need a command that fetches a fresh one, so nobody pastes tokens. Claude Code's [`apiKeyHelper`](https://code.claude.com/docs/en/llm-gateway-connect) setting runs a shell command that prints the credential, sends its output as both `Authorization: Bearer` and `x-api-key`, caches it for five minutes (`CLAUDE_CODE_API_KEY_HELPER_TTL_MS` changes that), and re-runs it after the cache lifetime. `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` take precedence over the helper, so leave both unset (and run `wye claude` without `--credential-file`). The helper can be distributed through managed settings. Wye only verifies: each new credential costs one full verification per connection, then the [per-connection cache](policy.md#gateway-credentials) applies.

**GitHub Actions**, with no secret in the repository: the job's OIDC token is the gateway credential. Wye trusts the issuer ([`[[auth.issuers]]`](policy.md#gateway-credentials)):

```toml
[[auth.issuers]]
issuer = "https://token.actions.githubusercontent.com"
audience = "wye-prod"
algorithm = "RS256"
jwks_file = "github.jwks"          # https://token.actions.githubusercontent.com/.well-known/jwks
subject_claim = "repository"       # usage per repository
[[auth.issuers.grants]]
claim = "repository_owner"
value = "acme"
models = ["claude-sonnet-*", "claude-haiku-*"]
```

The job asks GitHub for a token with Wye's audience on every helper run:

```sh
#!/bin/sh
# scripts/wye-credential.sh: prints a fresh GitHub Actions OIDC token for Wye.
curl -fsS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
  "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=wye-prod" | jq -r .value
```

```yaml
permissions:
  id-token: write                  # lets the job request OIDC tokens
  contents: read
steps:
  - uses: actions/checkout@v4
  - run: |
      mkdir -p .claude
      echo '{"apiKeyHelper": "sh scripts/wye-credential.sh"}' > .claude/settings.local.json
      claude -p "fix the failing tests"
    env:
      ANTHROPIC_BASE_URL: https://wye.internal.example.com
      CLAUDE_CODE_API_KEY_HELPER_TTL_MS: "240000"   # refresh within the token's lifetime
```

GitHub does not document the token lifetime (its example token is valid for five minutes), so keep the helper's cache lifetime below it; Wye rejects an expired token with `401`, and `max_credential_ttl_secs` bounds what it accepts.

**Developers** use the same setting with a helper that prints a token from an issuer Wye trusts: the team's IdP (an `[[auth.issuers]]` entry), or an internal service that authenticates the developer by SSO and mints a short-lived Wye credential with `wye token issue` (the signing key stays in that service). Offboarding then happens at the IdP: the next renewal fails, and the last token expires on its own ([revocation](policy.md#revocation)).

## Deployment modes

| User | Topology | Integration |
|---|---|---|
| Solo developer | Local subprocess / native binary | `wye claude` |
| Small team | Internal container or VM behind HTTPS | SDK base URL + gateway credential |
| Web backend on multi-region Kubernetes | Deployment + ClusterIP Service per cluster/region | Backend → cluster-local Wye DNS |
| Public AI app | Backend sidecar or shared internal Service | App owns user quotas; Wye routes inference |
| Serverless (e.g. Vercel) | Compatible serverless adapter or separate gateway service | Backend → Wye |
| Edge | Future WASM adapter | Platform-managed transport and scaling |

For a public app, a shared internal Deployment is preferred when many app replicas share a gateway; a sidecar per Pod gives stronger locality and independence.

**Kubernetes.** A minimal Helm release declares Deployment, ConfigMap, Secret references, Service, readiness/liveness probes, shutdown grace, resources, and optional OTel environment. No Kubernetes client or peer discovery in Wye. CPU-only HPA can mislead for I/O-bound SSE; an external metrics adapter can use active streams and memory.

**Reverse proxies.** Nginx and app servers in front of SSE must disable proxy buffering and use a server model suited to long-lived streams (e.g. ASGI rather than blocking WSGI workers).

**Serverless.** A native Tokio listener cannot be dropped into every serverless runtime; a gateway function must use the platform's streaming APIs. App databases and auth (e.g. Supabase) remain the app's concern. A platform's built-in AI gateway may be simpler for elementary routing; Wye's case is portability, transparent policy, and customer-controlled hosting.

**Edge.** A portable core with a platform adapter can run at the edge, but edge is not automatically faster: provider and telemetry location, network hops, and runtime limits matter. Do not promise identical native and WASM performance.

### Without Kubernetes

Statelessness makes every multi-node setup the same shape: a load balancer in front of identical processes reading the same config. Realistic options:

| Platform | Shape |
|---|---|
| EC2 Auto Scaling group + ALB | Instances fetch the binary and `wye.toml` (S3) and the provider key (SSM) at boot; instance refresh rolls deploys |
| ECS / Fargate | One service with N tasks; the load balancer health check targets `/readyz` |
| Docker Swarm | `docker service create --replicas N`, configs and secrets built in, `--update-order start-first` |
| Nomad | One job with `count = N`, Consul health checks |
| VMs + HAProxy or Caddy | systemd units; config from Ansible or Git |
| Fly.io-style regional hosting | The native binary in several regions behind anycast: the practical form of "edge" today |

For rolling updates without dropped streams, give the platform a stop timeout above `drain_delay_secs + shutdown_grace_secs`. Load balancers in front must not buffer SSE, and their idle timeout must exceed silent gaps in a stream (around 300 s; the AWS ALB default of 60 s is too short).

For container platforms, `upstream.api_key_file` reads the provider key from a mounted secret file at startup (Docker and Kubernetes secrets, systemd credentials; relative to the config file, surrounding whitespace trimmed), and [`wye probe`](#server) is the health check for images without `curl`. Both are startup or client-side, so the gateway stays stateless.

### Sizing (estimate)

From the TLS-path benchmark on a laptop, not yet measured on EC2: a heavy Claude Code user (about 5 concurrent streams, 250 events/s, 150 KB/s of resent context) costs Wye about 10 ms of CPU per second, about 1% of a core, and under 100 KB of memory. A 2-vCPU instance would serve roughly 130 heavy or 400 typical users at 70% CPU. Provider rate limits bind first; run two small instances for availability rather than capacity. Confirm with `wye-bench` on the target instance type and a separate load machine.

**Token rate.** Wye does not lower a stream's tokens per second: it relays each SSE event as it arrives, adding a roughly constant delay per event (about 0.15 ms p50 at light load, about 1.5 ms p50 with 2,000 streams at 100k events/s on the laptop) that does not accumulate along the stream. At 50–100 tokens/s the provider's 10–20 ms between tokens dominates; the first token arrives about 0.45 ms later. Measured capacity per core (`wye-bench load --scenario capacity`: Wye with one Tokio worker thread, TLS + HTTP/2, 100-byte events at 50 events/s): about 1,500 concurrent streams, 36,000 events/s, while the p99 delay Wye adds stays under 10 ms; beyond that the single thread queues (added p99 67–79 ms at 3,000 streams) before its CPU is used up, so latency, not CPU, sets the limit. CPU per event is lower on one thread (10–20 µs) than with all cores (about 35 µs in the gated run), so the multi-core runtime spends part of its CPU on coordination; size by measured streams per core, not by CPU per event. Two worker threads held 3,000 streams within the same bound; beyond that the laptop's load generator, not Wye, ran out of cores, so multi-core figures need a separate load machine. An Anthropic delta often carries more than one token. Model permissions and cache diagnostics add cost per request and per uploaded MB, not per streamed event ([research.md](research.md#current-harness-and-baseline)).

### Several Anthropic accounts

A team whose usage outgrows one Anthropic account's rate limits, or that splits spend across accounts, lists them under `[routes.claude]` instead of `[upstream]`:

```toml
[routes.claude]
[[routes.claude.upstreams]]
id = "team-a"
weight = 2                                   # twice the sessions of team-b
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_KEY_TEAM_A"
[[routes.claude.upstreams]]
id = "team-b"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_KEY_TEAM_B"
```

Each Claude Code session stays on one account, chosen from its session id (SDK callers without one: from the credential subject), so prompt caches keep hitting; every replica makes the same choice ([cache-affine routing](architecture.md#cache-affine-routing)). Weights share sessions, not tokens. Adding an account moves only the sessions it now wins; removing one moves only its own, and their next turn writes the cache again at the new account. No failover yet ([M4](roadmap.md#m4-claude-across-providers-and-accounts) stage 11): a `429` from one account goes back to the client, whose retry carries the same session id and reaches the same account. `wye explain --session <id>` shows which account a session uses; records, OTLP, and `wye report` split usage and cost by `upstream`.

## Telemetry

One usage record per proxied request, written as a JSON line to stderr and, with `[telemetry] otlp_endpoint`, exported as OTLP over HTTP with JSON encoding: a log record and a span per request, plus cumulative metrics. No Prometheus endpoint; a collector is optional. Records never contain prompts, completions, or credentials. Telemetry is not a quota ledger: records can be lost and counters reset ([policy.md](policy.md#state-boundary)).

**Usage record** (`wye serve 2>usage.jsonl` keeps the log; `wye report` reads it):

| Field | Values | Notes |
|---|---|---|
| `event`, `unix_ms`, `path`, `status` | `request`, time, route, HTTP status | |
| `outcome` | `completed` \| `failed` \| `cancelled` \| `gateway_error` | Recorded when the response body is dropped, so every outcome is counted |
| `duration_ms`, `headers_ms` | milliseconds | `headers_ms` = time to response headers |
| `upstream` | the route upstream's `id` | With `[routes.claude]`; absent with `[upstream]` and for requests refused before routing |
| `subject` | credential subject | With `[auth]` |
| `session_id` | Claude Code session | Kept only as a bounded token (below) |
| `requested_model` | the [alias](architecture.md#model-aliases-and-upgrade-checks) the client named | Only when an alias was resolved |
| `model`, `usage_status` | reported model; `complete` \| `partial` \| `unknown` \| `not_observed` | Streaming and non-streaming (JSON) responses are read; `not_observed`: errors, `count_tokens`, other content types |
| `upgrade_issues` | target model → rules it would reject | Checked requests only ([upgrade checks](architecture.md#model-aliases-and-upgrade-checks)); `{}` when none |
| `cost_usd`, `cost_status`, `pricing_version` | exact decimal string; `complete` \| `partial` \| `unknown`; `pricing.version` | With [`[pricing]`](policy.md#pricing-metadata); `cost_usd` only when a cost was computed |
| `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` | counts | As the provider reports them, except that on `/v1/responses` the cached share of OpenAI's `input_tokens` is moved to `cache_read_input_tokens`, so `input_tokens` excludes cache reads on both APIs |
| `request_class` | `main` \| `subagent` \| `workflow` \| `compaction` \| `auxiliary` \| `other` | Claude Code's hint header; for Codex, from `x-openai-subagent` ([mapping](architecture.md#codex-integration)) |
| `agent_type`, `agent_id`, `parent_agent_id`, `prompt_id` | tokens | Claude Code: `agent_id`s on every subagent request; the rest need `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`. Codex: thread ids as agent ids, the subagent kind as `agent_type`; no prompt id |
| `compaction`, `context_compacted` | `auto` \| `manual` \| `reactive` \| `other` | Hint headers |
| `tool_durations` | `[{"name": "Bash", "ms": 742}]` | Tool calls since the agent's previous request; at most 32 |
| `tool_calls` | `{"Bash": 2, "Read": 1}` | `tool_use` blocks the response started; at most 8 names, the rest under `(other)`; inputs never read |
| `cache_miss` | `compaction` \| `after_compaction` \| `unexplained` | A cache write with no cache read; a session's first request also reads `unexplained` |
| `prompt` | keyed prompt hashes | With [`[cache_diagnostics]`](research.md#2-cache-miss-diagnostics), sampled sessions only: `status`, `tools` and `system` (`count`, `hash`), `messages`, `breakpoint_count`, up to 4 `breakpoints`, the last 8 `message_prefixes`; 16-hex HMAC-SHA256 tags |

Client-supplied values are labels, never authorization: closed vocabularies map to fixed strings, and ids outside 1–128 bytes of letters, digits, `-`, `_`, `.`, `:` are dropped. Planned fields: `provider` (with a second provider, M4), `ttft_ms`, `config_version`.

**OTLP logs:** each record as a log record at `{otlp_endpoint}/v1/logs`, resource `service.name` (`telemetry.service_name`, default `wye`) and `service.version`. Attributes follow OpenTelemetry semantic conventions where they exist (`gen_ai.system`: `anthropic` or `openai` by endpoint, `gen_ai.response.model`, `gen_ai.usage.*_tokens`, `http.route`, `http.response.status_code`, `session.id`, `enduser.id`), then `claude_code.*` for the agent fields (Codex's mapped values too, under the same names for now) and `wye.*` for the rest (`wye.upstream`, `wye.requested_model`, `wye.cost_usd`, `wye.cost_status`, `wye.pricing_version`; `wye.tool_calls`, `wye.prompt`, and `wye.upgrade_issues` as JSON strings).

**OTLP metrics:** cumulative since process start, exported every 10 s and at shutdown; at most 256 attribute sets each; per process, summed by the backend.

| Metric | Kind | Attributes |
|---|---|---|
| `wye.requests` | sum | `http.route`, `wye.upstream`, `http.response.status_code`, `wye.outcome` |
| `wye.request.duration` | histogram, ms (5 ms to 300 s) | route, outcome |
| `wye.tokens` | sum | `gen_ai.token.type`, `gen_ai.response.model`, `claude_code.request_class`, `wye.upstream` |
| `wye.cost` | sum, USD | `gen_ai.response.model`, `claude_code.request_class`, `wye.upstream`; complete and partial costs, with `[pricing]` |
| `wye.upgrade.issues` | sum | `wye.upgrade.target`, `wye.upgrade.rule`, `enduser.id`; checked requests a target would reject |
| `wye.tool.calls` | sum | `claude_code.tool.name` |
| `wye.cache.misses` | sum | `wye.cache_miss` |
| `wye.requests.active` | gauge | |
| `wye.telemetry.lost` | sum | records lost to full queues, failed exports, or the series cap |

**OTLP traces:** one server span per request (`POST /v1/messages`) with the record's attributes, a `response headers` event, and `ERROR` status for failures and 5xx. Parent: a valid `traceparent` header; else, with a Claude Code session id, the session's trace (trace id = first 16 bytes of SHA-256 of the prefixed session id, the same on every replica, so a whole session is one timeline); else a new trace. Each reported tool call becomes a sibling span `tool <name>` ending when the request starts (approximate: Claude Code's times exclude prompts and hooks). No synthetic prompt or subagent spans: no single request knows it is the first. The `traceparent` sent upstream is the caller's.

**Bounds:** records pass a 4,096-record queue to one writer task; OTLP batches go at 512 records or after 1 s through a queue of 8 batches, with a 10 s timeout per export. Anything dropped is counted and written to stderr at shutdown (`usage_log_dropped`, `otlp_dropped`). `telemetry.otlp_headers_env` names an environment variable of extra export headers (`key=value,key2=value2`), treated as secrets; the export client trusts the upstreams' roots, every `ca_file` included. `wye.upstream` is present only with `[routes.claude]`.

## Health and shutdown

`/healthz` and `/readyz` report whether Wye can serve under its configured policy, not whether a monitoring backend is reachable. A transient OTel outage does not block inference.

On SIGTERM, shutdown has two phases:

1. **Draining** (`server.drain_delay_secs`, default 0): `/readyz` returns 503 while `/healthz` stays 200, and new connections and requests are still served, so a load balancer removes the replica before connections are refused. Set it to cover the load balancer's probe interval and endpoint propagation (in Kubernetes, typically 5–15 s) instead of a `preStop` sleep.
2. **Grace** (`server.shutdown_grace_secs`, default 30): the listener closes, active requests may finish, and whatever remains is cancelled; cancelling a stream closes its upstream request.

Set the pod's `terminationGracePeriodSeconds` above the sum of both.

## Failure behavior

| Event | Behavior |
|---|---|
| Bad config or missing secret | Fail startup with an actionable diagnostic, without exposing secrets |
| Invalid or expired credential | Reject locally |
| No eligible route | Explicit compatibility or policy error |
| Slow downstream | Bounded flow control: about 0.56 MB of heap per stalled stream (256 KiB upstream window), at most `max_concurrent_requests` of them; streams sharing its upstream connection are unaffected; a client that accepts no bytes for `client_write_timeout_secs` (60) is disconnected and its upstream request cancelled |
| Upstream failure before downstream output | Gateway error; later, bounded fallback where [safe](architecture.md#retries-and-fallback) |
| Partial stream failure | Terminate the stream; never silently replay output. A successful event stream that stalls past `stream_idle_timeout_secs` or breaks at an event boundary ends with an Anthropic `error` event (`overloaded_error`) and a clean close, so clients see a retryable API error; mid-event failures still drop the stream, since appending would corrupt the event |
| Client disconnect | Cancel upstream where possible, free resources, record uncertain cost |
| Telemetry export failure | Inference continues; loss visible in logs/counters |
| SIGTERM | Stop admissions, drain until deadline, cancel remainder |
| One replica dies | Others accept new connections; its streams fail |
| Fleet restart | Redeploy config and secrets; in-flight inference lost, no quota state to restore |
| Mixed config versions during rollout | Report version; decisions stay request-consistent |
