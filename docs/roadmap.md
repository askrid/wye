# Roadmap

Status, build order, open work, and exit criteria. Each milestone leaves a runnable system with focused tests. Git log has what was built when; [CHANGELOG.md](../CHANGELOG.md), what each release contains.

## Status

Release `0.1.0` is tested and benchmarked on one machine, its Linux service and image checked on Ubuntu 24.04, and in use with Claude Code on a claude.ai subscription since 2026-10-30 (streaming, tool use, prompt caching, extended thinking, token counting, background sessions).

Work continues in this order:

1. Fewer expensive tokens ([M2c](#m2c-fewer-expensive-tokens)).
2. Provider-held budgets ([M2](#m2-cost-policy-for-claude)).
3. The live checks: M0's API-key session, M3's Codex session, M4's live requests, and `wye service` on Linux.
4. The cost ceiling (M2).
5. A Compose example ([M1](#m1-team-ready-claude-code)).
6. Narrowable tokens (M1).
7. Vertex AI and AWS SigV4 ([M4](#m4-claude-across-providers-and-accounts)).

Later come model routing by subagent type, beyond size ([research](research.md#1-agent-aware-routing)), a Helm chart, and other agents on their native APIs.

### M0: Claude Code through Wye

Done and gated; the API-key half of the exit is open.

Exit: a real Claude Code session, with an API key and with a claude.ai login through passthrough, works through Wye with streaming, tool use, prompt caching, extended thinking, and token counting.

### M1: team-ready Claude Code

Wye tokens and identity-provider tokens, model permissions and output-token ceilings, `apiKeyHelper` renewal ([operations.md](operations.md#token-renewal)), the usage scanner, OTLP logs, metrics, and traces, `wye claude`, `wye service`, `wye probe`, secrets from files (`{ file = "..." }`), an OCI image with a smoke test, and release archives, a shell installer, and a published image (`ghcr.io/askrid/wye`) from a `v*` tag are built.

These are open:

- A Compose example and a Helm chart (internal service, health probes, secret references).
- Every secret from a file too (`hash_key_file` for cache diagnostics and `otlp_headers_file` for OTLP), since `wye service` cannot pass environment secrets ([running as a service](operations.md#running-as-a-service)).
- A deny list in config (token subjects or key ids), so offboarding need not wait for the last token to expire; it takes effect on the next restart or rollout ([revocation](policy.md#revocation)).
- A Docker Swarm service updating without dropping streams.
- [Narrowable tokens](research.md#3-narrowable-tokens-for-agent-swarms) for a subagent or a CI job.

Exit: a team runs Claude Code through a shared Wye with per-developer gateway tokens and sees usage per developer and session in its existing [OpenTelemetry backend](glossary.md#usage-and-cost).

### M1b: agent-aware visibility

Retry headers, `GET /v1/models`, spend by [request class](glossary.md#usage-and-cost), cache-miss reasons, session traces, `wye report` (over a time window too), and tool calls are built.

These are open:

- Report-only secret detection, sampled per session. It needs a dependency decision and a measured overhead per MB, and values never leave the process ([background](research.md#what-the-request-scanner-could-also-find)).
- Usage records gain the time to first token, the config version (to see a rollout's mixed window), and the provider's request id (for support tickets).
- `wye report` by subject and by model.

Exit: a team sees, in its own OpenTelemetry backend or with `wye report`, each Claude Code session as a structure (prompts, subagents, compactions, tools) with cost by request class and every unexpected cache miss flagged, with no gateway state and no gated metric worse.

### M2: cost policy for Claude

The price table ([policy.md](policy.md#price-table)) in records, OTLP, `wye report`, and `wye explain` is built; so are cache diagnostics.

These are open:

- Provider-held budgets: the provider's own spend ledger caps each team, with no Wye state:
  - Each budget owner (team, project, environment) gets its own provider workspace and API key, with a spend limit set at the provider.
  - An upstream lists the token subjects it serves (patterns, as model permissions use). Failover stays among that owner's upstreams, since another's would spend another budget.
  - An exhausted limit comes back as an explicit budget error.
  - Limits are as strict as the provider makes them (a hard stop or only an alert, possibly late); check Anthropic's first ([research](research.md#verification-needed)). With the cost ceiling, overshoot is bounded by the requests in flight at the cutoff.
  - Prompt caches are per account, so budget owners share no cache, which is usually the intended isolation.
- The [cost ceiling](research.md#6-provable-cost-bound) as a runaway guard for Claude models whose bounds are verified, capped by the context window; everything else labelled estimated-only; off by default on the Claude Code route.
- `wye explain` and policy tests report which upstream and budget a request uses, and the cost decision.

Exit: a team's monthly Claude spend is capped by Anthropic workspace limits with no Wye state, and every request reports a provable or explicitly estimated cost bound next to its actual cost.

### M2b: model aliases and upgrade checks

Done ([architecture.md](architecture.md#model-aliases-and-upgrade-checks)); the exit is open.

Exit: a team moves a model alias for all its services by one reviewed config change, after the upgrade check showed no failing client.

### M2c: fewer expensive tokens

Not started. In agent traffic most prompt tokens are already cache reads; the spend that can be cut is context carried from turn to turn, cache rebuilds, and large models on small background work ([evidence](research.md#where-the-tokens-go)). Wye never shortens or compresses a prompt.

- **Waste report.** `wye report` names the expensive tokens and prices a fix for each, offline, from the usage log:
  - Context carried past a threshold, and what clearing or compacting earlier would have cost.
  - Context each tool's output added, from the tool-time hint, as the prompt's growth between two requests times the requests that re-read it until compaction, at the cache-read price ("Bash output re-read 180 times cost $41").
  - Cache rebuilds with their cause, from cache diagnostics.
  - The cache TTL, 5 minutes or 1 hour, that would have cost least for this user's idle gaps, for subagents too ("subagent cache writes expired unread: $X; a 1h TTL would save $Y"), named as Claude Code's `promptCacheTtl` and `subagentPromptCacheTtl` settings.
  - Background and subagent requests on large models.
  - Tool definitions sent on every request and never called.

  Usage records gain the two inputs the report lacks, cache writes split by TTL (the usage scanner already parses it) and the size of the tools section.
- **Size-aware model routing.** An opt-in policy that sends small requests of a class (auxiliary first) to a smaller model through the allowlisted `model` rewrite.
  - It decides from headers and the body's length, so every replica agrees and nothing is read beyond what an alias already reads.
  - It never touches a main-thread request, or a request large enough to be reading another model's cache.
  - Off by default until quality is measured per request class ([research](research.md#1-agent-aware-routing)); `wye explain` shows the decision.
- **Savings estimate.** A request whose model Wye changed records what the same tokens would have cost at the requested model's price. `wye report` and an OTLP metric sum these, with the reason, as "saved by Wye", apart from "waste found" (what the report's fixes would save, which Wye does not claim). Savings with no per-request counterfactual, such as cache hits that cache affinity keeps, are not counted.

As decided on 2026-11-01, Wye does not add cache markers to app requests that send none, a rewrite beyond the allowlist. The waste report names those requests and prices what markers would save.

Exit: on real logs, every line of waste the report names comes with a priced fix, and every saved dollar it shows traces to usage records; size-aware model routing saves measurable spend on auxiliary requests with no measured loss of quality; no gated metric worse.

### M3: Codex

The OpenAI route, Responses and Chat Completions passthrough (with a provider key or the caller's own OpenAI API key), refusals in the OpenAI error shape, model permissions and output-token ceilings, usage and cost on all three APIs including non-streaming JSON, `wye codex` ([architecture.md](architecture.md#codex-integration)), Codex's headers mapped onto the same record fields, and stored responses reaching their upstream by cache affinity are built ([upstream-held state](architecture.md#upstream-held-state)).

These are open:

- A real Codex workflow streaming and calling tools through Wye (needs an OpenAI key), with tool-event fixtures from it.
- The `include_usage` decision ([open design decisions](architecture.md#open-design-decisions)).
- The OpenAI SDK with equivalent unary and streaming behavior.
- Codex on a ChatGPT login, passed through as Claude Code's claude.ai login is, once its endpoint and terms are checked ([research](research.md#verification-needed)).

Exit: a team runs Claude Code and Codex through one Wye, under one policy, and sees both in one telemetry model.

### M4: Claude across providers and accounts

Routes with several [upstreams](glossary.md#requests-and-where-they-go), cache affinity with standby upstreams, failover before output, and Claude Platform on AWS and Bedrock with API keys are built ([selection and failover](architecture.md#upstream-selection-and-failover), [providers](architecture.md#claude-across-providers)).

These are open (the auth questions are in [open design decisions](architecture.md#open-design-decisions)):

- One live request each on Bedrock and Claude Platform on AWS; contract tests per provider against recorded responses.
- An opt-in smoke test per provider against the real service, for SDK compatibility; it does not replace the deterministic protocol tests.
- SigV4 for Bedrock and Claude Platform on AWS.
- Google Vertex AI: model in the URL, the `anthropic_version` splice (property-tested to keep every other byte unchanged), access tokens.
- Re-reading a provider key file (`provider_key = { file = "..." }`) when it changes, so short-lived provider keys (Claude Platform on AWS issues 12-hour ones) need no restart. The config itself still changes only by restart.
- Excluding an upstream for a model or feature it does not serve.

Exit: a team serves Claude Code from at least two providers, keeps prompt-cache hit rates within a few points of a single provider, rides out one provider's overload without failed turns, and sees cost per provider.

### M5: benchmarks and optimization

The [commit gate](development.md#commit-gate) (since M0, with provider, failover, and rejection variants) and the agentgateway, Bifrost, LiteLLM, and KrakenD comparisons started early. Numbers from three [reference-environment](benchmarks.md#reference-environment) runs of one commit are published ([results](benchmarks.md#results)).

These are open:

- A floor baseline: nginx or Envoy streaming with `proxy_buffering off`, the overhead of a proxy that does nothing.
- Replay of the public TraceLab coding-agent trace for context sizes, output lengths, turn gaps ([related work](research.md#related-work)).
- A separate load machine, since the load generator saturates first at multi-core load (the reference environment gives the gateway 4 vCPUs of 16); then 10k streams.
- Soak (`wye-bench load --scenario paced`, for hours) and capacity per core (`--scenario capacity`) in the reference environment, whose published results have neither yet.
- A mock upstream that varies time to first token (token rate, stream length, and a rejection rate already vary).
- The next competitors, each in passthrough at its best case, with its license, version, and Claude Code support checked first and the result and configuration recorded in [benchmarks.md](benchmarks.md):
  - LiteLLM's Rust gateway, next: its July 2026 benchmark claims 0.7 ms p99 overhead against Bifrost's 4.5 ms.
  - Envoy AI Gateway: the infrastructure-standard data plane with LLM routing.
  - Helicone AI Gateway (Rust): the same language and speed pitch.
  - Portkey gateway (Node): a popular open-source gateway.
- Targeted optimization of memory, time to first token, backpressure, and CPU.

Exit: comparative results document actual performance, workload assumptions, and tradeoffs, with no speedup goal that a measurement does not back.

### M6: optional extensions

`wye policy-diff`, the CI half of [queryable policy](research.md#5-queryable-and-provable-policy), is done. An edge (WASM) adapter, [signed usage receipts](research.md#4-signed-usage-receipts), the rest of queryable policy, connect-only hedging, and other request-local experiments are not started.

Exit: each extension preserves or explicitly revises statelessness and compatibility guarantees.

## Open findings

Found by review rounds ([development.md](development.md#review-rounds)) and not fixed yet, with the reason. The next round starts here.

- No minimum read rate: a client that accepts one window update a minute keeps its stream, its permit, and the memory of a [stalled stream](benchmarks.md#current-harness-and-baseline), since the write timeout resets on any progress. Open: bounded by `max_concurrent_requests`; a rate floor needs a per-stream throughput measure, and slow mobile links are legitimate.
- Upstream HTTP/2 connections send no keepalive pings. A network path that dies silently holds its requests until `response_header_timeout` (`"10m"`), or `stream_idle_timeout` (`"2m"`) once a stream has started. Open: some front ends answer frequent pings with `GOAWAY`; needs a check against each provider.
- Chat Completions' `n` asks for several choices, each up to the output-token ceiling, so one request may generate `n` times the ceiling ([policy.md](policy.md#per-request-ceilings)). Open: reading `n` is one more field under the hold; refuse or multiply is a policy choice.
- Model permissions read only the top-level `model`; a model named inside the body (a Responses `image_generation` tool's) is not checked ([policy.md](policy.md#model-permissions)). Open: each such field is provider-specific; a list of them is a design change.
- OTLP metric series are first come, first served (256 per metric). One client can fill the tool-name or subject series for the life of the process. Open: the excess is counted in `wye.records.dropped`; per-attribute caps need a design.
- A chunked body whose chunk-size lines are padded (HTTP/1.1 allows leading zeros) can pin 8-32 KiB of the connection's read buffer per held or kept chunk, which counts 1 KiB, so under such a client the hold and failover budgets bound their charges, not memory. Open: a strict bound copies each held or kept chunk, one more allocation per request (gated), or caps chunk-size framing, which hyper does not expose; it takes an authenticated client uploading gigabytes.
- A client that stalls mid-upload after the hold is reported as an upstream timeout (504) after `response_header_timeout` (`"10m"`), and keeps its permit and, on a failover route, its share of the kept-body budget. Decided (2026-11-02): no idle timer on uploads, which would add a timer to every request and a new way for slow uploads to fail.
- `wye-bench` stopped by a signal sent to it alone (a `kill` of its pid, not a terminal's Ctrl-C, which reaches its gateways too) leaves its gateways running. Open: a signal handler (Tokio's `signal` feature) would stop them; a Ctrl-C, the usual way to stop a gate run, already does.
- No `cargo-fuzz` targets for the parsers of untrusted input; property tests cover the scanners. Open: needs a nightly toolchain, a decision for the maintainer.
- `wye service uninstall` ignores any `bootout` failure (exit 3 means not loaded). Open: launchctl's other failure modes are unverified.
- Idle upstream sockets after a burst stay until the peer closes them, since no `pool_timer` is set. Open: the fix needs a way to test it without a 90 s wait.
- `wye doctor` does not warn about a provider key file others can read. Mounted secrets are often 0444, so it would need to tell them apart.
- `wye claude` starts the agent with an expired gateway token, which then gets 401s. The token's `exp` could be read locally first.
- The report has no cost column until the config has `[pricing]`, which `wye init` does not write ([why](policy.md#price-table); [visibility ideas](research.md#visibility-for-one-developer)).

## First release (0.1)

What must be true to tag it:

- [x] The release workflow publishes a GitHub Release from a `v*` tag ([`v0.1.0-rc.1`](https://github.com/askrid/wye/releases/tag/v0.1.0-rc.1)).
- [x] `gh attestation verify` passes on its archives.
- [x] `wye service` has run on a real Linux machine (Ubuntu 24.04 on EC2, 2026-11-12).
- [ ] One full Claude Code session with an API key. Not run for 0.1.0; the README says it is tested only against mocks.
- [x] A container image on GHCR, published by the release workflow from 0.1.0 on.

CONTRIBUTING, issue forms, Discussions, and code of conduct are postponed while solo-maintained.

## Before 1.0

Every [invariant](architecture.md#invariants) and [security requirement](policy.md#security-requirements) is held by a test, and:

- [ ] Each endpoint × provider × feature in the support matrix passes conformance and integration tests, and the matrix is generated from their results.
- [ ] Claude Code and Codex at their target versions, Chat Completions with OpenAI, and each Claude provider pass contract tests against the live service.
- [ ] The image, the Compose example, and the Helm release need no datastore.
- [ ] Cost-saving claims are backed by usage, pricing, and task-quality evidence.
- [x] Model permissions and output-token ceilings read the body as strict UTF-8 JSON, as the provider's parser does; a body another decoder could read differently (another charset, a content encoding, bytes outside JSON, keys differing only in case) is refused (`crates/gateway-core/src/policy/tests.rs`, `crates/wye/tests/gateway/gateway_tokens.rs`).
- [x] `wye explain` answers as `serve` does, word for word, and an endpoint no route serves is refused (`crates/wye/tests/gateway/explain.rs`).
- [x] Shutdown drains streams within a configured deadline (`crates/wye/tests/gateway/lifecycle.rs`).
- [x] `wye claude` and `wye codex` start their agents against the gateway without changing the user's own configuration (`crates/wye/src/cli/wrapper.rs`).
