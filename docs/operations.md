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
wye explain --config wye.toml [--credential token.txt] [--session <id>] [--path <path>] request.json
wye token keygen --out issuer.key
wye token issue --key issuer.key --kid k1 --issuer wye --audience wye-prod --subject team-research --ttl-secs 3600
wye policy-test --config wye.toml policy-tests/
wye report usage.jsonl
wye probe http://127.0.0.1:8080/readyz
wye doctor
```

Implemented: `serve`, `check-config`, `token`, `policy-test`, `report`, `probe`, and a first `explain`; `doctor` is planned.

`probe` sends one `GET` over plain HTTP and exits 0 only for a 2xx answer within 5 s: a health check for images without `curl` (Docker `HEALTHCHECK CMD ["wye", "probe", "http://127.0.0.1:8080/readyz"]`).

`doctor` (planned) checks local config, resolved environment variables, DNS/TLS/provider connectivity, model availability, and compatibility warnings, without an admin API.

**Implemented:** `explain` prints each check `serve` would make for the request and the decision, calling the same functions in the same order: endpoint, credential verification (at the current time), body size, the model permission (requested model, the configured and credential lists), the upstream URL and provider credential (by name, never value), and whether cache diagnostics would hash it. It exits 0 for "forward" and 3 for a refusal, with the status and error type the server would send. Conditions that exist only at run time (the concurrency limit, the held-bytes budget, upstream failures) are not evaluated. With routing and cost ceilings (later milestones), `explain` grows to the full design:

`explain` runs the exact admission and routing code used by `serve`, offline, and prints the decision: selected route and candidate, rejected candidates with reasons, rewritten fields, provable cost bound, and the provider credential (by name, never value) that would be used. It makes no network calls. Because routing is a pure function of request, credential, and config, the output is what every replica would do.

Policy becomes testable in CI: commit sample requests with expected decisions, and run them on every `wye.toml` change. A policy change that silently reroutes traffic or raises a cost bound fails review.

**Implemented: `wye policy-test --config wye.toml <dir>`.** Each `<name>.json` in the directory is a request body and `<name>.expect.toml` states the decision: `decision = "forward"` or `"refuse"`, optionally `status`, `error`, `path`, `session`, and a `[credential]` table (`subject`, `models`, `max_output_tokens`). Tokens would expire in CI, so a case states the credential's claims and only the signature check is skipped; every later check is the server's, through `explain`. It prints one line per case and exits 3 if any fails. [`examples/policy-tests/`](../examples/policy-tests) has a config and three cases (the background Haiku model allowed, Opus above the configured ceiling refused, no credential refused), and the test suite runs them.

**Implemented: `wye report [<usage.jsonl>]`.** Summarizes a usage log (the JSON lines `wye serve` writes to stderr; standard input by default) with no telemetry backend: requests and outcomes; tokens, cache-hit rate (cache reads as a share of prompt tokens), request time, and tool time by request class; cache misses split into unexplained ones, session starts, compactions, and the first request after a compaction (a session's first request is an expected miss that one request cannot recognize, but the whole log can); tool calls the model requested, and calls and time Claude Code reported, by tool; and the ten sessions and prompts with the most tokens. Other lines are skipped and counted. `wye serve --config wye.toml 2>usage.jsonl` keeps the log; `wye report usage.jsonl` reads it.

```text
policy-tests/
├── background-haiku.json          # request
├── background-haiku.expect.toml   # decision = "forward", [credential] models = [...]
├── opus-beyond-ceiling.json
└── opus-beyond-ceiling.expect.toml  # decision = "refuse", status = 403
```

With routing and cost ceilings, expectations will also name the route and bound a cost (`route = "codex"`, `max_cost_usd <= "0.05"`).

`token issue` signs gateway credentials offline from a private key file ([policy.md](policy.md#gateway-credentials)). It never runs inside the server. `--models <name,prefix*,...>` limits the credential to those models, and `--max-output-tokens <n>` lowers its `max_tokens` ceiling.

### Client wrapper

**Implemented: `wye claude`.** `wye claude [--gateway <url>] [--credential-file <path>] [--] [claude args...]` checks that the gateway answers `/healthz`, sets `ANTHROPIC_BASE_URL` (default `http://127.0.0.1:8080`, or `WYE_GATEWAY_URL`) and, with a gateway credential (`--credential-file` or `WYE_CREDENTIAL`), `ANTHROPIC_AUTH_TOKEN` while removing `ANTHROPIC_API_KEY`, and then replaces itself with `claude` (Unix `exec`): signals, the terminal, and the exit code are Claude Code's own, and nothing is written, so nothing needs cleaning up. Without a credential, Claude Code keeps its own login (subscription passthrough). `claude` is found on `PATH` (a shell alias is not applied; `WYE_CLAUDE_BIN` names another program), and arguments after `--` go to Claude Code unchanged. Profiles, `wye codex`, and `wye run` below are proposals.

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

```bash
wye claude
wye --profile team claude -p "fix tests"
wye codex
wye run -- my-custom-agent
```

The wrapper resolves profile and credentials, detects a supported child tool, generates **temporary** config or environment overrides, launches the child with signal and exit-code forwarding, and cleans up.

An optional `wye connect claude` may edit client config transactionally with backups and a matching `disconnect`; ephemeral wrapping is the default. Client-side processes launched by the wrapper do not violate the single-purpose gateway principle.

### SDK integration

```python
from openai import OpenAI

client = OpenAI(
    base_url="https://gateway.example.com/v1",
    api_key="gateway-credential",
)
```

Claude Code connects the same way:

```bash
export ANTHROPIC_BASE_URL=https://gateway.example.com
export ANTHROPIC_AUTH_TOKEN=gateway-credential   # or apiKeyHelper for automatic renewal
```

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

## Telemetry

Wye emits OTLP. External systems collect, store, alert, and visualize. A collector is optional if the backend accepts OTLP directly. No separate Prometheus `/metrics` endpoint.

The table and field list below are the target; the **Implemented** paragraphs after them describe what exists.

| Signal | Examples |
|---|---|
| Metrics | Requests, active streams, latency histograms, TTFT, upstream failures, token counts |
| Traces | Validation, selected route, upstream request, stream duration and cancellation |
| Usage events | Subject/project, provider/model, tokens, cached tokens, cost estimate, accounting status, pricing version |
| Errors | Auth rejection, incompatible feature, malformed SSE, provider timeout, export failure |

Target usage event fields (cost and pricing fields wait for M2): `request_id`, `timestamp`, `subject_id`, `project_id`, `client_session_id`, `client_agent_id`, `provider`, `model`, `endpoint`, `input_tokens`, `output_tokens`, `cached_tokens`, `estimated_usd`, `max_cost_usd` (provable bound), `provider_reported_usd`, `pricing_version`, `accounting_status` (`confirmed` | `estimated` | `unknown`), `duration_ms`, `ttft_ms`, `config_version`.

**Implemented: usage records as OTLP logs.** With `[telemetry] otlp_endpoint`, every usage record is also exported as an OTLP log record over OTLP/HTTP with JSON encoding (`POST {otlp_endpoint}/v1/logs`), with resource attributes `service.name` (`telemetry.service_name`, default `wye`) and `service.version`. Attributes use OpenTelemetry semantic conventions where they exist: `gen_ai.system` (`anthropic`), `gen_ai.response.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_creation.input_tokens`, `gen_ai.usage.cache_read.input_tokens`, `http.route`, `http.response.status_code`, `session.id` (Claude Code session), `enduser.id` (credential subject); `claude_code.request_class`, `claude_code.agent_type`, `claude_code.agent_id`, `claude_code.parent_agent_id`, `claude_code.prompt_id`, `claude_code.compaction`, `claude_code.context_compacted`, `wye.tool_calls` (the object as a JSON string), and `wye.cache_miss` from the usage-log fields above; and `wye.outcome`, `wye.duration_ms`, `wye.headers_ms`, `wye.usage_status`, and `wye.prompt` (the cache-diagnostics object as a JSON string) otherwise. The writer task batches records (sent at 512 records or after one second) and hands batches to an exporter task through a queue of 8 batches; a full queue, a failed request, or a collector slower than 10 s drops the batch, and the number of lost records is written to stderr at shutdown (`otlp_dropped`). `telemetry.otlp_headers_env` names an environment variable with extra request headers in the OpenTelemetry format `key=value,key2=value2` (for example a backend API key); values are treated as secrets. The export client trusts the same roots as the upstream (`upstream.ca_file` included). 

**Implemented: OTLP metrics.** The same writer task aggregates the records into cumulative metrics (since process start) and exports them to `{otlp_endpoint}/v1/metrics` every 10 s and at shutdown: `wye.requests` (sum, by `http.route`, `http.response.status_code`, `wye.outcome`), `wye.request.duration` (histogram in ms, by route and outcome; buckets 5 ms to 300 s), `wye.tokens` (sum, by `gen_ai.token.type` = input, output, cache_read, cache_creation, `gen_ai.response.model`, and `claude_code.request_class` when the hint headers are on: what main turns, subagents, compactions, and background calls cost), `wye.tool.calls` (sum, by `claude_code.tool.name`: what agents ask to run), `wye.cache.misses` (sum, by `wye.cache_miss` reason), `wye.requests.active` (gauge of requests whose response has not ended), and `wye.telemetry.lost` (records lost to full queues or failed exports, plus records beyond the series limit). Cumulative totals make a lost export harmless: the next one carries them. Each metric keeps at most 256 attribute sets. These are per-process numbers; replicas never compare or merge them, the backend sums them.

**Implemented: OTLP traces.** Each usage record is also exported as a server span (`POST /v1/messages`) to `{otlp_endpoint}/v1/traces`, batched with the log records: start and end time, the record's attributes, a `response headers` event (time to first byte), and status `ERROR` for failed or gateway-error outcomes and 5xx statuses. A valid W3C `traceparent` request header makes the span its child, so it joins the caller's trace. Otherwise a request with `x-claude-code-session-id` joins its session's trace: the trace id is the first 16 bytes of SHA-256 of the session id (with a fixed prefix), which every replica computes alike, so a backend shows a whole Claude Code session (main turns, subagents, compactions, background calls) as one timeline with no coordination; the agent attributes tell the requests apart. A request without either starts a new trace. Each tool call a request reports in `x-claude-code-prev-tool-durations` becomes an internal span `tool <name>` (attributes `claude_code.tool.name`, `claude_code.tool.duration_ms`, `session.id`, `claude_code.agent_id`, `claude_code.prompt_id`), a sibling of the request span that ends when the request starts: an approximation, since Claude Code's times exclude permission prompts and hooks, and parallel calls overlap. Wye emits no synthetic parent spans for prompts or subagents: a parent would have to be emitted by exactly one request, which no single request can know it is. The `traceparent` forwarded upstream is still the caller's (Wye does not insert its own span into the provider request yet).

**Usage log.** Wye also writes one JSON line per proxied request to stderr: `event`, `unix_ms`, `path`, `status`, `outcome` (`completed` | `failed` | `cancelled` | `gateway_error`), `duration_ms`, `headers_ms`, `subject` (the credential subject, with `[auth]`), `session_id`, `model`, `usage_status` (`complete` | `partial` | `unknown` | `not_observed`), and the reported token counts (`input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`). Claude Code's agent headers add `request_class` (`main` | `subagent` | `workflow` | `compaction` | `auxiliary` | `other`), `agent_type`, `agent_id`, `parent_agent_id`, `prompt_id`, `compaction`, `context_compacted` (`auto` | `manual` | `reactive` | `other`), and `tool_durations` (`[{"name": "Bash", "ms": 742}, …]`: the tool calls that ran since the agent's previous request, from `x-claude-code-prev-tool-durations`; names percent-decoded, at most 32) when present; all but the agent ids need `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` on the client. They are labels, never authorization: closed vocabularies are mapped to fixed strings, and ids or agent types outside 1–128 bytes of letters, digits, `-`, `_`, `.`, `:` are dropped. `tool_calls` (`{"Bash": 2, "Read": 1}`) counts the `tool_use` blocks the response started, by tool name: at most 8 names of up to 64 token characters per response, the rest under `(other)`; tool inputs are never read. `cache_miss` marks a request the provider reports as writing the prompt cache without reading it: `compaction` (the summarization request), `after_compaction` (the first main request after one), or `unexplained` (a regression worth a look, or a session's first request, which one request cannot recognize: exclude those in the backend by session). With `[cache_diagnostics]`, a `prompt` object adds keyed prompt hashes for [cache-miss diagnostics](research.md#2-cache-miss-diagnostics): `status` (`complete` | `unknown`), `tools` and `system` (`count`, `hash`), `messages` (count), `breakpoint_count`, `breakpoints` (up to four: `section`, `index`, `block` for messages, `hash`), and `message_prefixes` (the last eight: `index`, `hash` of `messages[0..=index]`). Hashes are 16 hex digits of HMAC-SHA256 under the configured key, never raw content hashes. With `sample_percent` below 100, only requests of sampled sessions carry the object, and requests without a session id carry none. Records pass through a bounded queue (4096) to one writer task; on overflow they are dropped and counted, and the count is written at shutdown. The log never contains prompts, completions, or credentials.

Keep high-cardinality IDs out of metric labels. Never export prompts, completions, credentials, or full tool arguments by default. Export queues are bounded and asynchronous. Counters reset and events can be lost: **telemetry is not a quota ledger** ([policy.md](policy.md#state-boundary)).

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
| Slow downstream | Bounded flow control |
| Upstream failure before downstream output | Gateway error; later, bounded fallback where [safe](architecture.md#retries-and-fallback) |
| Partial stream failure | Terminate the stream; never silently replay output. A successful event stream that stalls past `stream_idle_timeout_secs` or breaks at an event boundary ends with an Anthropic `error` event (`overloaded_error`) and a clean close, so clients see a retryable API error; mid-event failures still drop the stream, since appending would corrupt the event |
| Client disconnect | Cancel upstream where possible, free resources, record uncertain cost |
| Telemetry export failure | Inference continues; loss visible in logs/counters |
| SIGTERM | Stop admissions, drain until deadline, cancel remainder |
| One replica dies | Others accept new connections; its streams fail |
| Fleet restart | Redeploy config and secrets; in-flight inference lost, no quota state to restore |
| Mixed config versions during rollout | Report version; decisions stay request-consistent |
