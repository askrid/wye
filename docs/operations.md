# Operations

Distribution, CLI, deployment, telemetry, and failure behavior. The gateway binary is an ordinary inference server: IaC, DNS, certificates, infrastructure secrets, scheduling, and autoscaling are not implemented inside it.

## Distribution

- Versioned Linux and macOS binaries (x86-64, ARM64 as supported).
- Signed, versioned OCI image; no consumer Dockerfile required.
- Docker Compose example for a VM.
- Helm chart for Kubernetes.
- Terraform reference examples where useful.
- Later: Cloudflare Workers / Fastly Compute Rust/WASM adapter, subject to streaming and API constraints.

## CLI

### Server

```bash
wye serve --config wye.toml
wye check-config wye.toml
wye explain --config wye.toml --credential token.txt request.json
wye token keygen --out issuer.key
wye token issue --key issuer.key --kid k1 --issuer wye --audience wye-prod --subject team-research --ttl-secs 3600
wye doctor
```

`doctor` checks local config, resolved environment variables, DNS/TLS/provider connectivity, model availability, and compatibility warnings, without an admin API.

`explain` runs the exact admission and routing code used by `serve`, offline, and prints the decision: selected route and candidate, rejected candidates with reasons, rewritten fields, provable cost bound, and the provider credential (by name, never value) that would be used. It makes no network calls. Because routing is a pure function of request, credential, and config, the output is what every replica would do.

Policy becomes testable in CI: commit sample requests with expected decisions, and run them on every `wye.toml` change. A policy change that silently reroutes traffic or raises a cost bound fails review.

```text
policy-tests/
├── codex-tools.json        # request
├── codex-tools.expect.toml # route = "codex", candidate = "openai_responses", max_cost_usd <= "0.05"
├── image-hard-mode.json
└── image-hard-mode.expect.toml  # reject = "unbounded_cost"
```

`token issue` signs gateway credentials offline from a private key file ([policy.md](policy.md#gateway-credentials)). It never runs inside the server.

### Client wrapper (proposed)

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

## Telemetry

Wye emits OTLP. External systems collect, store, alert, and visualize. A collector is optional if the backend accepts OTLP directly. No separate Prometheus `/metrics` endpoint.

| Signal | Examples |
|---|---|
| Metrics | Requests, active streams, latency histograms, TTFT, upstream failures, token counts |
| Traces | Validation, selected route, upstream request, stream duration and cancellation |
| Usage events | Subject/project, provider/model, tokens, cached tokens, cost estimate, accounting status, pricing version |
| Errors | Auth rejection, incompatible feature, malformed SSE, provider timeout, export failure |

Suggested usage event fields: `request_id`, `timestamp`, `subject_id`, `project_id`, `client_session_id`, `client_agent_id`, `provider`, `model`, `endpoint`, `input_tokens`, `output_tokens`, `cached_tokens`, `estimated_usd`, `max_cost_usd` (provable bound), `provider_reported_usd`, `pricing_version`, `accounting_status` (`confirmed` | `estimated` | `unknown`), `duration_ms`, `ttft_ms`, `config_version`.

**Interim usage log.** Until OTLP export lands, Wye writes one JSON line per proxied request to stderr: `event`, `unix_ms`, `path`, `status`, `outcome` (`completed` | `failed` | `cancelled` | `gateway_error`), `duration_ms`, `headers_ms`, `session_id`, `model`, `usage_status` (`complete` | `partial` | `unknown` | `not_observed`), and the reported token counts. Records pass through a bounded queue (4096) to one writer task; on overflow they are dropped and counted, and the count is written at shutdown. The log never contains prompts, completions, or credentials.

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
| Partial stream failure | Terminate the stream; never silently replay output |
| Client disconnect | Cancel upstream where possible, free resources, record uncertain cost |
| Telemetry export failure | Inference continues; loss visible in logs/counters |
| SIGTERM | Stop admissions, drain until deadline, cancel remainder |
| One replica dies | Others accept new connections; its streams fail |
| Fleet restart | Redeploy config and secrets; in-flight inference lost, no quota state to restore |
| Mixed config versions during rollout | Report version; decisions stay request-consistent |
