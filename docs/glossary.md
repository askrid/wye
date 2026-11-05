# Glossary

One name for each thing Wye does. The docs, the website, the CLI, and the code use these terms; "Not" lists names that are retired for the concept, so a reader never has to wonder whether two words mean two things.

## The product

- **Wye**: the product. **The gateway**: a running Wye process. **Replica**: one of several identical gateways behind a load balancer. Not: the server, the proxy.
- **Config**: the one TOML file a gateway reads at startup (`wye.toml`); every option, with its default: [examples/wye.toml](../examples/wye.toml). Not: policy (for the whole file).
- **Setup**: what `wye init` writes, from two choices: who can reach the gateway (this machine, or other machines with gateway tokens) and whose credential pays (the caller's, or a provider key Wye holds). Three setups result: **passthrough** (the default: this machine, the caller's own login or key, no secret in Wye), **provider key** (`--provider-key`: this machine, Wye holds the provider key), **shared** (`--shared`: gateway tokens, every interface, the provider key from the deployment's environment). A shared gateway cannot pass callers' credentials through. Not: team setup, api-key setup.

## Requests and where they go

- **Endpoint**: an API path Wye serves, such as `/v1/messages` or `/v1/responses`. Not: route (for a path).
- **Native API**: a provider's own API, passed through without translation: Anthropic Messages, OpenAI Responses, OpenAI Chat Completions.
- **Route**: the upstreams that serve one native API: `[routes.claude]` (or its single-upstream form, `[upstream]`) and `[routes.openai]`.
- **Upstream**: one provider account a route sends requests to: an `id`, a provider, a base URL, and its provider key. Not: account (except to explain it), key, candidate, backend.
- **Provider**: who serves an upstream: `anthropic`, `claude-platform-aws`, `bedrock`, or `openai`. Not: provider for an identity provider.
- **Base URL**: where an upstream's requests go. Not: endpoint (for an upstream).
- **Provider key**: the key Wye presents to an upstream, read from an environment variable or a file. Not: credential, the operator's key, bare "key".
- **Upstream selection**: how a route picks the upstream for a request: `cache_affine` (the default) or `ordered`. Not: routing (for this).
- **Cache affinity**: the `cache_affine` selection: each session, else each subject, stays on one upstream, so its prompt cache stays warm. Its input is the **affinity key** (the session id, else the subject). Not: session affinity, sticky.
- **Rejection**: an upstream's answer that the request was not processed: `429`, `529`, or a connection that failed before the body was delivered. Not: refusal (that is Wye's).
- **Upstream error**: no usable answer from an upstream, so Wye answers `502` or `504` itself; its kind is `connect` (no connection or TLS handshake), `timeout` (no response headers in time), or `reset` (the request failed after connecting). Not: rejection (that is an answer), refusal.
- **Failover**: resending a rejected request to the route's next upstream, before any output, when the route has `failover` on. The rejection's kind is the **failover reason** (`429`, `529`, `connect`). Not: retry, fallback.
- **The hold**: Wye reading a request body until its top-level `model` is known, before it forwards anything, when a feature needs the model: model permissions, an output-token ceiling, an alias, a provider's own model id. Not: buffering.
- **Kept body**: the bytes of an upload Wye keeps so failover can resend them, up to the route's `failover_max_body_bytes` per request and `server.max_total_kept_bytes` across the process. Not: held (that is the hold).
- **Agent wrapper**: `wye claude` or `wye codex`, which start the agent pointed at the gateway. Not: client wrapper.

## Access

- **Gateway token**: what a caller presents to Wye instead of a provider key: a signed JWT, either a **Wye token** (from `wye token issue`) or an **identity-provider token** (from GitHub Actions, or SSO through `apiKeyHelper`). Wye replaces it with the provider key. Not: credential, scoped key. In a paragraph that also counts model tokens, always "gateway token".
- **Issuer**: who signs gateway tokens: Wye (with its **signing key**) or an identity provider. Not: signer, token service.
- **Identity provider (IdP)**: an external issuer configured in `[[auth.identity_providers]]`, whose keys Wye pins in config. Not: bare "provider".
- **Subject**: the identity a gateway token names (`sub`, or the issuer's `subject_claim`). Aliases roll out per subject, and cache affinity falls back to it.
- **Grant**: an `[[auth.identity_providers.grants]]` rule that gives an identity-provider token its models by the value of one of its claims.
- **Model permissions**: the models a request may use: the config's model list (`[auth] models`), narrowed by the token's `models` claim or by grants. Wye refuses any other model before upstream work. Not: allowlist, ceiling (for models).
- **Output-token ceiling**: the largest output-token value a request may ask for: the stricter of `[auth] max_output_tokens` and the token's claim. Not: limit, cap.
- **Caller credential**: the caller's own claude.ai login or provider API key. **Caller-credential passthrough**: Wye forwards it unchanged instead of a provider key (loopback only; the passthrough setup).

## Models

- **Alias**: a model name clients send that config maps to a model, per subject, through `[[aliases]]`; a `when` rule rolls a new model out to some subjects first. Not: stable name (as a term).
- **Requested model** and **served model**: the model a request named, and the model that ran after an alias (records: `requested_model`, `model`). Not: resolved model.
- **Model discovery**: `GET /v1/models` answered from the config's **catalog** (`[discovery]`), narrowed to the caller's model permissions, so Claude Code's model picker shows only usable models.
- **Upgrade check**: a report of which requests a **target model** would reject (`[upgrade_check]`, `wye explain --target-model`). Each finding is an **upgrade issue**: one rule the request breaks. Report only; the request is never changed.

## Decisions

- **Policy**: the rules in config that decide a request: model permissions, output-token ceilings, aliases, upstream selection, failover. Not: the whole config, retry policy.
- **Decision**: what Wye does with one request: **forward** it, or **refuse** it with an error the client reads. Not: reject (that is the upstream's).
- **Refusal**: Wye's own error answer, in the endpoint's error shape: `400`, `401`, `403`, `404`, `408`, `413`, or `415`, or `529`/`503` when it is overloaded.
- **Checks**: the offline commands that show or test what the gateway would do: `wye explain` (one request's decision), `wye policy-test` (expected decisions in CI), `wye policy-diff` (whether a config change widens access), `wye doctor` (secrets, files, and upstreams), `wye validate`.

## Usage and cost

- **Agent**: the client program, such as Claude Code or Codex. Within a session, the **main agent** and its **subagents**.
- **Session**: the client's session id: a whole Claude Code or Codex conversation.
- **Request class**: what a request is for: `main`, `subagent`, `workflow`, `compaction`, `auxiliary` (small background requests), or `other`. Not: work type.
- **Usage record**: one JSON line per request with its tokens, cost, served model, session, request class, and outcome; with `[telemetry]`, also an OTLP log record and span. Not: request record, log line, event.
- **Usage log**: where usage records go: stdout, or `~/.wye/usage.jsonl` under `wye service`. The **message log** (`~/.wye/wye.log`) holds Wye's own messages. Not: the log (alone), stdout log.
- **Cost**: dollars computed per request from the price table; **spend**: cost over time. **Overhead**: what Wye itself uses (latency, CPU, memory). Not: cost (for Wye's CPU or memory).
- **Price table**: `[pricing]`: prices per model and upstream, with a version every record carries. A model the table does not name costs `unknown`, never $0. Not: pricing metadata.
- **Cache miss**: a request that wrote the prompt cache and read nothing from it, recorded with its reason (`compaction`, `after_compaction`, `unexplained`); `wye report` counts session and subagent starts as expected.
- **Cache diagnostics**: `[cache_diagnostics]`: keyed hashes of a request's prompt sections, sampled per session, that name which section changed before a cache miss.
- **Telemetry**: OTLP export of usage records, metrics, and traces (`[telemetry]`), to your **OpenTelemetry backend**. Not: telemetry (for the usage log), observability backend.

## Development

- **Commit gate**: `scripts/check.sh`, run before each commit that changes code: formatting, lints, tests, builds, and a benchmark against the parent commit in alternating rounds ([development.md](development.md#commit-gate)). Not: benchmark gate, regression gate.

## Planned

Terms for designs that are not built yet; [roadmap.md](roadmap.md) has their status.

- **Cost ceiling**: a per-request limit on cost, refused before the provider is called when its bound would exceed it.
- **Provider-held budgets**: spend limits per team, enforced by the provider on each team's own key.
- **Narrowable tokens**: a gateway token whose holder can derive a narrower one for a subagent or a job, without the issuer.
- **Size-aware model routing**: sending small background requests to a smaller model, by rule.
