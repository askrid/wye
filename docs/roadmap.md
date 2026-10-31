# Roadmap

Each milestone leaves a runnable system with focused tests. Order of the next work: [product.md](product.md#next). What was built when: git log.

## Status

| Milestone | State |
|---|---|
| M0: Claude Code stream proxy | Done and gated. Full sessions run on a claude.ai login since 2026-10-30 (streaming, tool use, prompt caching, extended thinking, token counting, background sessions). Open: the exit check with an API key |
| Realistic-workload benchmark | Done: uploads, 2,000 token-paced streams, capacity per core, stress scenarios, KrakenD, LiteLLM, and Bifrost comparisons |
| M1: team-ready Claude Code | Built: Wye's own and identity-provider tokens, model permissions and output-token ceilings, `apiKeyHelper` renewal ([operations.md](operations.md#token-renewal)), usage scanner, OTLP logs, metrics, and traces, `wye claude`, `wye service` (the systemd path is tested but has not run on a real Linux machine), `wye probe`, `upstream.api_key_file`, local OCI image with a smoke test, release archives from a `v*` tag (first release pending). Open: published images, Compose example, a Docker Swarm service updating without dropping streams, the exit check |
| M1b: agent-aware visibility | Built: A1-A4 (retry headers, `GET /v1/models`, spend by work type, cache-miss reasons, session traces, `wye report`, tool calls). Open: A5 |
| M2: cost policy | Built: pricing ([policy.md](policy.md#pricing-metadata)) in records, OTLP, `wye report`, and `wye explain`; cache-miss diagnostics (opt-in, sampled per session). Not started: cost bound, provider-held budgets |
| M2b: model aliases and upgrade checks | Done ([architecture.md](architecture.md#model-aliases-and-upgrade-checks)). Open: the exit check, a real alias move |
| M3: Codex | Built: `[routes.openai]`, Responses and Chat Completions passthrough, OpenAI-shaped gateway errors, permissions and ceilings, usage and cost on all three APIs including non-streaming JSON, `wye codex` ([architecture.md](architecture.md#codex-integration)), Codex's headers mapped onto the same record fields, stored responses reaching their account by affinity ([as built](architecture.md#upstream-held-state)). Next: a real Codex workflow (needs an OpenAI key), tool-event fixtures from it, the `include_usage` decision |
| M4: Claude across providers and accounts | Built: routes with several accounts, `cache_affine` or `ordered`, failover before output, Claude Platform on AWS and Bedrock with API keys ([architecture.md](architecture.md#claude-across-providers-m4)). Open: one live Bedrock request, SigV4, Vertex; these need the auth decisions in [architecture.md](architecture.md#open-design-decisions) |
| M5: benchmarks and optimization | Started early: the commit gate since M0, with provider, failover, and refusal variants; KrakenD, LiteLLM, and Bifrost comparisons ([benchmarks.md](benchmarks.md)). Open: agent trace replay, a floor baseline, a separate Linux load machine, the remaining competitors |
| M6: optional extensions | `wye policy-diff` done (the CI half of [queryable policy](research.md#5-queryable-and-provable-policy)); the rest not started |

## Open work and exits

### M0: Claude Code stream proxy

Exit: a real Claude Code session, with an API key and with a claude.ai login through passthrough, works through Wye with streaming, tool use, prompt caching, extended thinking, and token counting. Half done: the API-key session is open.

### M1: team-ready Claude Code

Open: published images; a Compose example; a Docker Swarm service updating without dropping streams.

Exit: a team runs Claude Code through a shared Wye with per-developer tokens and sees usage per developer and session in its existing OTel backend.

### M1b: agent-aware visibility

Open: A5, report-only secret detection, sampled per session. It needs a dependency decision and a measured cost per MB, and values never leave the process ([background](research.md#agent-native-directions)).

Exit: a team sees, in its own OTel backend or with `wye report`, each Claude Code session as a structure (prompts, subagents, compactions, tools) with cost by work type and every unexpected cache miss flagged, with no gateway state and no gated metric worse.

### M2: cost policy for Claude

- The [provable cost ceiling](policy.md#provable-cost-ceiling) as a runaway guard for Claude models whose bounds are verified, capped by the context window; everything else labelled estimated-only. Off by default on the Claude Code route.
- [Provider-held budgets](policy.md#provider-held-budgets): identity → Anthropic workspace key mapping and explicit budget-exhausted errors.
- `wye explain` and policy tests report the cost decision.

Exit: a team's monthly Claude spend is capped by Anthropic workspace limits with no Wye state, and every request reports a provable or explicitly estimated cost bound next to its actual cost.

### M2b: model aliases and upgrade checks

Exit: a team moves a model alias for all its services by one reviewed config change, after the upgrade check showed no failing client.

### M3: Codex

Open: a real Codex workflow streaming and calling tools through Wye, with tool-event fixtures from it; the `include_usage` policy; the OpenAI SDK with equivalent unary and streaming behavior. Agent-aware telemetry for Codex uses whatever signals Codex sends; the record, metrics, and `wye report` stay one model across agents.

Exit: a team runs Claude Code and Codex through one Wye, under one policy, and sees both in one telemetry model.

### M4: Claude across providers and accounts

- One live Bedrock request; contract tests per provider against recorded responses.
- A small opt-in real-provider smoke test per provider (planned; none exists yet). It checks SDK compatibility and does not replace deterministic protocol tests.
- SigV4 for Bedrock and Claude Platform on AWS.
- Google Vertex AI: model in the URL, `anthropic_version` splice (property-tested: every other byte unchanged), access tokens.
- Agent-aware model routing within Claude ([research](research.md#1-agent-aware-routing)), once its quality impact is measured.
- A Helm chart.

Exit: a team serves Claude Code from at least two providers, keeps prompt-cache hit rates within a few points of a single provider, rides out one provider's overload without failed turns, and sees cost per provider.

### M5: benchmarks and optimization

Adds a floor baseline (nginx or Envoy as a plain streaming proxy), the [remaining competitors](benchmarks.md#benchmark-suite-kept-cut-missing), 10k streams, Linux and a separate load machine, replay of the public TraceLab coding-agent trace ([research.md](research.md#related-work)), and targeted optimization of memory, TTFT, backpressure, and CPU.

Exit: comparative results document actual performance, workload assumptions, and tradeoffs, with no speedup goal that a measurement does not back.

### M6: optional extensions

Edge/WASM adapter, [delegable credentials](research.md#3-delegable-credentials-for-agent-swarms), [signed usage receipts](research.md#4-signed-usage-receipts), the rest of [queryable policy](research.md#5-queryable-and-provable-policy), connect-only hedging, and other request-local experiments.

Exit: each extension preserves or explicitly revises statelessness and compatibility guarantees.

## Open findings

Found by review rounds ([development.md](development.md#review-rounds)) and not fixed yet, with the reason. The next round starts here.

| Finding | Why it is open |
|---|---|
| No minimum read rate: a client that accepts one window update a minute keeps its stream, permit, and about 0.7 MB, since the write timeout resets on any progress | Bounded by `max_concurrent_requests`; a rate floor needs a per-stream throughput measure, and slow mobile links are legitimate |
| Client connections are not capped, and `/healthz` and `/readyz` take no permit, so a connection flood is bounded only by the open-file limit | A connection cap is a new limit to size and document; accept errors already pause instead of spinning |
| Upstream HTTP/2 connections send no keepalive pings: a network path that dies silently holds its requests until `response_header_timeout_secs` (600 s) | Some front ends answer frequent pings with `GOAWAY`; needs a check against each provider |
| Model permissions read only the top-level `model`; a model named inside the body (a Responses `image_generation` tool's) is not checked ([policy.md](policy.md#gateway-tokens)) | Each such field is provider-specific; a list of them is a design change |
| OTLP metric series are first come, first served (256 per metric): one client can fill the tool-name or subject series for the life of the process | The excess is counted in `wye.telemetry.lost`; per-attribute caps need a design |
| Tests: some negative checks (`try_recv` on an empty channel) race the upstream, and a few tests wait with fixed sleeps | Test cleanup; none has flaked on the laptop or CI |
| Test and benchmark code repeats setup (recording upstreams, SSE responses, leaked `'static` bodies, CPU before-and-after pairs), and `wye-bench` leaves its gateways running when interrupted | Cleanup with no behavior change; next round |
| CI's benchmark rebuilds the reference commit from scratch on every run: its cache holds the build, but `bench-against.sh` removes a reference tree without a worktree | CI time only; the fix is a build directory outside the worktree |
| No `cargo-fuzz` targets for the parsers of untrusted input; property tests cover the scanners | Needs a nightly toolchain: a decision for the maintainer |
| `wye service uninstall` ignores any `bootout` failure (exit 3 means not loaded) | launchctl's other failure modes are unverified |
| Idle upstream sockets after a burst stay until the peer closes them, since no `pool_timer` is set | The fix needs a way to test it without a 90 s wait |

From the CLI review (2026-10-30), not fixed:

- `wye doctor` does not warn about a provider key file others can read: mounted secrets are often 0444, so it would need to tell them apart.
- `wye claude` starts the agent with an expired gateway token, which then gets 401s. The token's `exp` could be read locally first.
- `policy-diff` takes no `-` for a config (`git show main:wye.toml | wye policy-diff - wye.toml`): key sets are read relative to each config, so a worktree remains the right way.
- No `--json` on `report`, and no shell completions; both could come from what exists when someone needs them.
- `wye claude` does not read the config's `listen`, on purpose: the config is the server's, and the gateway may be remote. A changed port needs `WYE_URL`.
- A `gateway_error` record does not say why: `wye report` shows `502 6` on a real log, and neither the record nor the report tells a refused connection from a broken stream. Six in a burst of auxiliary requests (2026-10-30) stay unexplained for that reason.
- The report has no cost column until the config has `[pricing]`, and `wye init` writes none, since a table in the binary would need upkeep ([visibility ideas](research.md#visibility-for-one-developer)).

## First release (0.1)

What must be true to tag it:

- [ ] The release workflow publishes a GitHub Release from a `v*` tag.
- [ ] `gh attestation verify` passes on its archives.
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
- [ ] Caller-credential passthrough refuses to start on a non-loopback bind.
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
