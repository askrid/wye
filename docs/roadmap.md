# Roadmap

Each milestone leaves a runnable system with focused tests. Order of the next work: [product.md](product.md#next). What was built when: git log.

## Status

| Milestone | State |
|---|---|
| M0: Claude Code stream proxy | Done and gated. Full sessions run on a claude.ai login since 2026-10-30 (streaming, tool use, prompt caching, extended thinking, token counting, background sessions). Open: the exit check with an API key |
| Realistic-workload benchmark | Done: uploads, 2,000 token-paced streams, capacity per core, stress scenarios, agentgateway, Bifrost, LiteLLM, and KrakenD comparisons |
| M1: team-ready Claude Code | Built: Wye's own and identity-provider tokens, model permissions and output-token ceilings, `apiKeyHelper` renewal ([operations.md](operations.md#token-renewal)), usage scanner, OTLP logs, metrics, and traces, `wye claude`, `wye service` (the systemd path is tested but has not run on a real Linux machine), `wye probe`, `upstream.api_key_file`, local OCI image with a smoke test, release archives from a `v*` tag (`0.1.0-rc.1` published). Open: published images, Compose example, Helm chart, secrets from files for every key, a deny list, a Docker Swarm service updating without dropping streams, the exit check |
| M1b: agent-aware visibility | Built: A1-A4 (retry headers, `GET /v1/models`, spend by work type, cache-miss reasons, session traces, `wye report`, tool calls). Open: A5 |
| M2: cost policy | Built: pricing ([policy.md](policy.md#pricing-metadata)) in records, OTLP, `wye report`, and `wye explain`; cache-miss diagnostics (opt-in, sampled per session). Next after M2c: provider-held budgets. Not started: cost bound |
| M2b: model aliases and upgrade checks | Done ([architecture.md](architecture.md#model-aliases-and-upgrade-checks)). Open: the exit check, a real alias move |
| M2c: fewer expensive tokens | Next ([product.md](product.md#next)); not started. Evidence: [where the tokens go](research.md#where-the-tokens-go) |
| M3: Codex | Built: `[routes.openai]`, Responses and Chat Completions passthrough, OpenAI-shaped gateway errors, permissions and ceilings, usage and cost on all three APIs including non-streaming JSON, `wye codex` ([architecture.md](architecture.md#codex-integration)), Codex's headers mapped onto the same record fields, stored responses reaching their account by affinity ([as built](architecture.md#upstream-held-state)). Next: a real Codex workflow (needs an OpenAI key), tool-event fixtures from it, the `include_usage` decision |
| M4: Claude across providers and accounts | Built: routes with several accounts, `cache_affine` or `ordered`, failover before output, Claude Platform on AWS and Bedrock with API keys ([architecture.md](architecture.md#claude-across-providers-and-accounts)). Open: one live request each on Bedrock and Claude Platform on AWS, SigV4, Vertex; these need the auth decisions in [architecture.md](architecture.md#open-design-decisions) |
| M5: benchmarks and optimization | Started early: the commit gate since M0, with provider, failover, and refusal variants; agentgateway, Bifrost, LiteLLM, and KrakenD comparisons ([benchmarks.md](benchmarks.md)). Open: agent trace replay, a floor baseline, a separate Linux load machine, the remaining competitors |
| M6: optional extensions | `wye policy-diff` done (the CI half of [queryable policy](research.md#5-queryable-and-provable-policy)); the rest not started |

## Open work and exits

### M0: Claude Code stream proxy

Exit: a real Claude Code session, with an API key and with a claude.ai login through passthrough, works through Wye with streaming, tool use, prompt caching, extended thinking, and token counting. Half done: the API-key session is open.

### M1: team-ready Claude Code

- Published images, a Compose example, and a Helm chart (internal service, health probes, secret references).
- Every secret from a file too: `hash_key_file` for cache diagnostics and `otlp_headers_file` for OTLP, since `wye service` cannot pass environment secrets ([running as a service](operations.md#running-as-a-service)).
- A deny list in config (token subjects or key ids), so offboarding need not wait for the last token to expire; it takes effect on the next restart or rollout ([revocation](policy.md#revocation)).
- A Docker Swarm service updating without dropping streams.
- Narrowable tokens: a token holder hands a subagent or a CI job a narrower token (fewer models, a lower ceiling, a shorter life) without the issuer ([delegable credentials](research.md#3-delegable-credentials-for-agent-swarms)); sixth in [product.md](product.md#next)'s order.

Exit: a team runs Claude Code through a shared Wye with per-developer tokens and sees usage per developer and session in its existing OTel backend.

### M1b: agent-aware visibility

- A5, report-only secret detection, sampled per session. It needs a dependency decision and a measured cost per MB, and values never leave the process ([background](research.md#agent-native-directions)).
- Records gain the time to first token, the config version (to see a rollout's mixed window), and the provider's request id (for support tickets).
- `wye report` by subject and by model, and over a time window (`--since`).

Exit: a team sees, in its own OTel backend or with `wye report`, each Claude Code session as a structure (prompts, subagents, compactions, tools) with cost by work type and every unexpected cache miss flagged, with no gateway state and no gated metric worse.

### M2: cost policy for Claude

- **Provider-held budgets, next after M2c** ([design](policy.md#provider-held-budgets)). An upstream lists the token subjects it serves, so each team's requests use its own Anthropic workspace key and the spend limit set on it; failover stays among that team's keys, never another budget's; an exhausted limit comes back as an explicit budget error. First: check how hard and how fast Anthropic's workspace limits stop requests ([research](research.md#verification-needed)).
- The [provable cost ceiling](policy.md#provable-cost-ceiling) as a runaway guard for Claude models whose bounds are verified, capped by the context window; everything else labelled estimated-only. Off by default on the Claude Code route.
- `wye explain` and policy tests report which key and budget a request uses, and the cost decision.

Exit: a team's monthly Claude spend is capped by Anthropic workspace limits with no Wye state, and every request reports a provable or explicitly estimated cost bound next to its actual cost.

### M2b: model aliases and upgrade checks

Exit: a team moves a model alias for all its services by one reviewed config change, after the upgrade check showed no failing client.

### M2c: fewer expensive tokens

In agent traffic most prompt tokens are already cache reads at 5 to 10 percent of the input price; the spend that can be cut is context carried from turn to turn, cache rebuilds, and large models on small background work ([evidence](research.md#where-the-tokens-go)). Wye never shortens or compresses a prompt: a changed byte in a cached prefix turns those reads into writes at 12.5 to 40 times the price.

- **Waste report.** `wye report` names the expensive tokens and prices a fix for each, offline, from the log: context carried past a threshold and what clearing or compacting earlier would have cost; cache rebuilds with their cause (from [cache diagnostics](research.md#2-cache-miss-diagnostics)); the cache TTL, 5 minutes or 1 hour, that would have cost least for this user's idle gaps; background and subagent requests on large models; tool definitions sent on every request and never called. Records gain the two inputs the report lacks: cache writes split by TTL, and the size of the tools section.
- **Size-aware model routing.** An opt-in policy that sends small requests of a class (auxiliary first) to a smaller model through the allowlisted `model` rewrite. It decides from headers and the body's length, so every replica agrees and nothing is read beyond what an alias already reads. It never touches a main-thread request, or a request large enough to be reading another model's cache. `wye explain` shows the decision; the client sees it too, since the response names the model that ran, and records keep both the requested and the served model. Off by default until quality is measured per request class ([research](research.md#1-agent-aware-routing)).
- **Savings estimate.** A request whose model Wye changed records what the same tokens would have cost at the requested model's price. `wye report` and an OTLP metric show "saved by Wye", the sum of those estimates with the reason, apart from "waste found", what the report's fixes would save, which Wye does not claim. Savings with no per-request counterfactual, such as the cache hits that session affinity keeps, are not counted.

Decided (2026-11-01): Wye does not add cache markers to app requests that send none, a rewrite beyond the allowlist. The waste report names those requests and prices what markers would save.

Exit: on real logs, every line of waste the report names comes with a priced fix, and every saved dollar it shows traces to request records; size-aware routing saves measurable spend on auxiliary requests with no measured loss of quality; no gated metric worse.

### M3: Codex

Open: a real Codex workflow streaming and calling tools through Wye, with tool-event fixtures from it; the `include_usage` policy; the OpenAI SDK with equivalent unary and streaming behavior; Codex on a ChatGPT login, passed through as Claude Code's claude.ai login is, once its endpoint and terms are checked ([research](research.md#verification-needed)). Agent-aware telemetry for Codex uses whatever signals Codex sends; the record, metrics, and `wye report` stay one model across agents.

Exit: a team runs Claude Code and Codex through one Wye, under one policy, and sees both in one telemetry model.

### M4: Claude across providers and accounts

- One live request each on Bedrock and Claude Platform on AWS; contract tests per provider against recorded responses.
- A small opt-in real-provider smoke test per provider (planned; none exists yet). It checks SDK compatibility and does not replace deterministic protocol tests.
- SigV4 for Bedrock and Claude Platform on AWS.
- Google Vertex AI: model in the URL, `anthropic_version` splice (property-tested: every other byte unchanged), access tokens.
- Re-read an `api_key_file` when it changes, so short-lived keys (Claude Platform on AWS issues 12-hour ones) need no restart. The config itself still changes only by restart.
- Excluding an upstream for a model or feature it does not serve.

Exit: a team serves Claude Code from at least two providers, keeps prompt-cache hit rates within a few points of a single provider, rides out one provider's overload without failed turns, and sees cost per provider.

### M5: benchmarks and optimization

Adds a floor baseline (nginx or Envoy as a plain streaming proxy), the [remaining competitors](benchmarks.md#benchmark-suite-kept-cut-missing), 10k streams, Linux and a separate load machine, replay of the public TraceLab coding-agent trace ([research.md](research.md#related-work)), and targeted optimization of memory, TTFT, backpressure, and CPU.

Exit: comparative results document actual performance, workload assumptions, and tradeoffs, with no speedup goal that a measurement does not back.

### M6: optional extensions

Edge/WASM adapter, [signed usage receipts](research.md#4-signed-usage-receipts), the rest of [queryable policy](research.md#5-queryable-and-provable-policy), connect-only hedging, and other request-local experiments.

Exit: each extension preserves or explicitly revises statelessness and compatibility guarantees.

## Open findings

Found by review rounds ([development.md](development.md#review-rounds)) and not fixed yet, with the reason. The next round starts here.

| Finding | Why it is open |
|---|---|
| No minimum read rate: a client that accepts one window update a minute keeps its stream, permit, and about 0.7 MB, since the write timeout resets on any progress | Bounded by `max_concurrent_requests`; a rate floor needs a per-stream throughput measure, and slow mobile links are legitimate |
| Client connections are not capped, and `/healthz` and `/readyz` take no permit, so a connection flood is bounded only by the open-file limit | A connection cap is a new limit to size and document; accept errors already pause instead of spinning |
| Upstream HTTP/2 connections send no keepalive pings: a network path that dies silently holds its requests until `response_header_timeout_secs` (600 s), or `stream_idle_timeout_secs` (120 s) once a stream has started | Some front ends answer frequent pings with `GOAWAY`; needs a check against each provider |
| Chat Completions' `n` asks for several choices, each up to the output-token ceiling, so one request may generate `n` times the ceiling ([policy.md](policy.md#per-request-ceilings)) | Reading `n` is one more field under the hold; refuse or multiply is a policy choice |
| Model permissions read only the top-level `model`; a model named inside the body (a Responses `image_generation` tool's) is not checked ([policy.md](policy.md#gateway-tokens)) | Each such field is provider-specific; a list of them is a design change |
| OTLP metric series are first come, first served (256 per metric): one client can fill the tool-name or subject series for the life of the process | The excess is counted in `wye.telemetry.lost`; per-attribute caps need a design |
| Tests: a few negative checks (`try_recv` on an empty channel, in aliases, OpenAI, and failover tests) could race the upstream, and some tests wait with fixed sleeps, among them a refusal mid-upload in the failover tests (200 ms for the refusal to arrive first) | Test cleanup; none has flaked on the laptop or CI. The checks that the upstream never got a refused body whole now wait a bounded 200 ms |
| A chunked body whose chunk-size lines are padded (HTTP/1.1 allows leading zeros) can pin 8-32 KiB of the connection's read buffer per held or kept chunk, which counts 1 KiB, so under such a client the hold and failover budgets bound their charges, not memory | A strict bound copies each held or kept chunk, one more allocation per request (gated), or caps chunk-size framing, which hyper does not expose; it takes an authenticated client uploading gigabytes |
| A client that stalls mid-upload after the hold is reported as an upstream timeout (504) after `response_header_timeout_secs` (600 s), and keeps its permit and, on a failover route, its share of the kept-body budget | Decided (2026-11-02): no idle timer on uploads, which would add a timer to every request and a new way for slow uploads to fail; the wait stays bounded by `response_header_timeout_secs` |
| Test and benchmark code repeats setup (recording upstreams, SSE responses, leaked `'static` bodies, CPU before-and-after pairs), and `wye-bench` leaves its gateways running when interrupted | Cleanup with no behavior change; next round |
| CI's benchmark rebuilds the reference commit from scratch on every run: its cache holds the build, but `bench-against.sh` removes a reference tree without a worktree | CI time only; the fix is a build directory outside the worktree |
| No `cargo-fuzz` targets for the parsers of untrusted input; property tests cover the scanners | Needs a nightly toolchain: a decision for the maintainer |
| `wye service uninstall` ignores any `bootout` failure (exit 3 means not loaded) | launchctl's other failure modes are unverified |
| Idle upstream sockets after a burst stay until the peer closes them, since no `pool_timer` is set | The fix needs a way to test it without a 90 s wait |

From the CLI review (2026-10-30), not fixed:

- `bench/baseline.json` lacks the ten `*_sse_failed_streams` metrics: it is a record, not a gate input, so it waits for the next refresh on an idle machine.

- `wye doctor` does not warn about a provider key file others can read: mounted secrets are often 0444, so it would need to tell them apart.
- `wye claude` starts the agent with an expired gateway token, which then gets 401s. The token's `exp` could be read locally first.
- `policy-diff` takes no `-` for a config (`git show main:wye.toml | wye policy-diff - wye.toml`): key sets are read relative to each config, so a worktree remains the right way.
- No `--json` on `report`, and no shell completions; both could come from what exists when someone needs them.
- `wye claude` does not read the config's `listen`, on purpose: the config is the server's, and the gateway may be remote. A changed port needs `WYE_URL`.
- A `gateway_error` record does not say why: `wye report` shows `502 6` on a real log, and neither the record nor the report tells a refused connection from a broken stream. Six in a burst of auxiliary requests (2026-10-30) stay unexplained for that reason.
- The report has no cost column until the config has `[pricing]`, and `wye init` writes none, since a table in the binary would need upkeep ([visibility ideas](research.md#visibility-for-one-developer)).

## First release (0.1)

What must be true to tag it:

- [x] The release workflow publishes a GitHub Release from a `v*` tag ([`v0.1.0-rc.1`](https://github.com/askrid/wye/releases/tag/v0.1.0-rc.1)).
- [x] `gh attestation verify` passes on its archives.
- [ ] `wye service` has run on a real Linux machine, or the release notes say the systemd path is untested.
- [ ] One full Claude Code session with an API key.
- [ ] A container image on GHCR.

Postponed while solo-maintained: CONTRIBUTING, issue forms, Discussions, SECURITY.md, code of conduct.

## Before 1.0

### Functional / API

- [ ] Every supported endpoint × provider × feature combination has passing conformance and integration tests.
- [ ] The published support matrix is generated from those test results, not written by hand.
- [ ] Anthropic Messages passes the Claude Code gateway contract tests for the target Claude Code version.
- [ ] Native OpenAI Responses streaming works with the target Codex version.
- [ ] Chat Completions unary and streaming work with OpenAI.
- [ ] Each Claude provider passes the Messages contract tests; failover happens only before any response byte, never after output.
- [ ] Aliases resolve the same in `explain` and `serve`; upgrade checks report and never change a request.
- [ ] Unsupported provider/endpoint combinations reject explicitly.
- [ ] No unbounded buffering on SSE with a slow downstream.
- [ ] Cancellation frees gateway resources and does not claim to cancel already billed usage.

### Policy / security

- [ ] Signature, issuer, audience, expiry, and model permissions enforced before upstream dispatch.
- [ ] Model permissions and ceilings read the body as strict UTF-8 JSON, as the provider's parser does; a body another decoder could read differently (another charset, a content encoding, bytes outside JSON, keys differing only in case) is refused.
- [ ] Config references and environment secrets validated; errors fail closed.
- [x] Caller-credential passthrough refuses to start on a non-loopback bind (config test `rejects_non_loopback_listen`).
- [ ] Provider credentials and prompt bodies not logged by default.
- [ ] Hard per-request cost mode rejects unbounded or unknown billable dimensions.
- [ ] Hard cost mode is enabled only for models whose tokens-≤-bytes bound and reasoning-token billing are verified.
- [ ] Only allowlisted fields are rewritten; caller limits are never raised.
- [ ] `wye explain` output matches `serve` decisions for the same request, token, and config.
- [ ] Token replay documented and not represented as preventable without redemption state.
- [ ] No global/monthly budget or global RPM enforcement advertised.

### Operations / DX

- [ ] `wye serve` works from a prebuilt binary.
- [ ] OCI image and Compose example need no gateway datastore.
- [ ] Helm release exposes Wye internally with health probes and secret references.
- [ ] An SDK backend switches using base URL plus gateway token.
- [ ] `wye claude` and `wye codex` launch their agents against the gateway without changing the user's own configuration.
- [ ] An OTel backend shows usage and cost by subject, model, and session without a custom dashboard.
- [ ] Rollout and shutdown drain streams within a configured deadline.

### Benchmarks

- [ ] Mock upstream varies TTFT, token rate, failure rate, and stream length. (Token rate, stream length, and a refusal rate are built; TTFT is not.)
- [ ] Reports include p50/p95/p99 added latency, RSS, CPU, and slow-consumer behavior.
- [ ] Competitor tests use comparable features and disclose differences.
- [ ] Cost-saving claims are backed by usage, pricing, and task-quality evidence.
