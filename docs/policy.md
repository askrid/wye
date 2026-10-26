# Security and policy

What Wye authorizes and enforces per request, what it cannot enforce without shared state, and its security requirements.

## Gateway credentials

Provider credentials authenticate Wye to providers; they do not authenticate callers to Wye. Callers present signed gateway credentials. Without an `[auth]` section Wye listens on loopback only; with one it may listen on any address, and every proxied request must carry a valid credential.

Claims of Wye's own credentials (`max_request_cost_usd` is planned for M2; the others are implemented; identity-provider tokens are below):

```json
{
  "iss": "wye",
  "sub": "team-research",
  "aud": "wye-prod",
  "iat": 1791996400,
  "exp": 1792000000,
  "models": ["claude-sonnet-*", "claude-haiku-*"],
  "max_output_tokens": 4096,
  "max_request_cost_usd": "0.05"
}
```

The issuer signs credentials outside the runtime, with `wye token issue` (offline, from a private key file) or an external issuer producing the same format. Replicas hold verification keys only, selected by key ID so keys can rotate. All checks run before contacting upstream. Accept the credential from `Authorization: Bearer` or `x-api-key`; Claude Code sends whichever credential variable the developer set.

**Implemented (M1 stage 4a):** compact JWTs with `alg` pinned to `EdDSA`, a JWT-compatible encoding so identity-provider tokens (stage 4c) share one strict parser.

- **Rejected with `401 authentication_error`, before any upstream contact:** another `alg`, an unknown header field (including `crit`), a `kid` not in `[auth.keys]` or an issuer's key set, a bad signature, the wrong `iss` or `aud`, expiry, `iat`/`nbf` more than 60 s ahead, or `exp - iat` above `max_credential_ttl_secs` (default one week). Claims are parsed only after the signature verifies.
- **Read from** `Authorization: Bearer` or `x-api-key`; replaced by the provider key, never forwarded.
- **Cached per connection:** a request repeating the last verified credential skips signature verification, compares the bytes in constant time, and rechecks expiry. The cache holds one verified credential and dies with the connection; keys are fixed for the process lifetime, so its answers equal a fresh check.
- **Cost:** a full verification adds ~88 µs of CPU (the Ed25519 check is 55–62 µs); on a reused connection, authenticated requests cost the same as unauthenticated ones (`auth_` metrics).

**The algorithm is fixed by the key**, never chosen by the token: Ed25519 for Wye's own credentials, one configured algorithm per identity provider. General JWT algorithm negotiation (`alg: none`, HMAC/RSA confusion) is a well-known source of verification bugs; a pinned `alg` removes that class entirely while keeping the encoding readable by ordinary JWT tooling.

**Implemented (M1 stage 4b): model permissions.** `[auth] models` lists the models any credential may use; a credential's `models` claim (`wye token issue --models claude-sonnet-*,claude-haiku-*`) narrows it, never widens it. An entry is an exact name or a prefix ending in `*`, which covers dated ids and Claude Code's background Haiku model. With [model aliases](architecture.md#model-aliases-and-upgrade-checks), permissions apply to the resolved model.

- **The hold:** with either list, Wye holds the request body only until its top-level `model` is read (Claude Code sends it first, so usually one chunk), within `server.max_held_request_bytes` (default 64 MiB, all requests together; beyond it `529`). Without either list (or an alias, a ceiling), the body is not read.
- **Refusals, before upstream contact:** a forbidden model gets `403 permission_error`; a missing, non-string, escaped, or repeated `model`, an escaped top-level key, or a body that is not a JSON object gets `400 invalid_request_error`.
- **The tail check:** JSON allows a repeated key, and a provider might honor a later `"model"`, so the rest of the body is lexed as it streams; a second top-level `model`, or an escaped top-level key, fails the body and aborts the upstream request. It costs CPU per MB on structure-dense bodies ([research.md](research.md#current-harness-and-baseline)) and can go if the provider is verified to reject duplicate keys.

**Limit precedence.** When the credential and the config (`[auth]`) both set a limit, the stricter value applies. A credential can narrow configured policy, never widen it.

**Implemented (M1 stage 4c): identity-provider tokens.** Wye also accepts JWTs issued directly by a trusted OIDC identity provider, so teams need no token-minting service: GitHub Actions or another workload identity for CI, and the team's IdP (through `apiKeyHelper`) for developers. Each `[[auth.issuers]]` entry pins `issuer`, `audience`, one `algorithm` (`RS256`, `ES256`, or `EdDSA`; never HMAC, never read from the token), and `jwks_file`, the provider's published JSON Web Key Set committed next to the config and read at startup (keys of other types, curves, algorithms, or marked for encryption are skipped). Wye's own `issuer`, `audience`, and `[[auth.keys]]` become optional: a deployment can trust only its IdP.

- **Key selection.** A token's `kid` selects its key, and every key belongs to one issuer; a `kid` used by two issuers, or by an issuer and `[[auth.keys]]`, fails startup. The header must name that issuer's algorithm, and after the signature verifies the claims must name that issuer, so claims are still parsed only after the signature. Provider headers may carry extra fields (`typ`, `x5t`), which are ignored; `crit` is rejected.
- **Claims.** `iss`, `aud`, `exp`, and `iat` are required, with the same rules as Wye's own tokens (`max_credential_ttl_secs`, 60 s skew for `iat`/`nbf`). `subject_claim` (default `sub`; for example `email`, or `repository` for GitHub Actions) names the subject in the usage log and telemetry; a token without it is rejected. A provider's `models` or `max_output_tokens` claims are ignored: limits come from config.
- **Grants.** `[[auth.issuers.grants]] claim = "groups", value = "ml-team", models = ["claude-sonnet-*"]` grants models to tokens whose claim equals the value or is a list containing it. A token gets the union of its matching grants, still capped by `[auth] models`. With grants configured, a token matching none may use no model (`403 permission_error`); an issuer without grants leaves the `[auth] models` ceiling in charge.
- **Tradeoffs.** RSA and ECDSA verification and a generic claims object are a larger surface than the fixed Ed25519 format, so the algorithm is pinned per issuer and keys come only from config. Pinned keys need a config change when the IdP rotates them: commit the new set before the IdP starts signing with it (providers publish upcoming keys ahead of use). Fetching the JWKS at runtime would remove that step but adds network I/O and cached state; it stays a later, explicit option. A full verification happens once per connection, as for Wye's own tokens (the per-connection cache).

Macaroons or Biscuit allow attenuation (team → project → subagent) without contacting an issuer; not needed so far, and tracked as [delegable credentials](research.md#3-delegable-credentials-for-agent-swarms).

A token can assert a **maximum per request**, never the cumulative amount spent. Short TTLs reduce exposure but do not make credentials single-use. Proof of possession helps against theft but not against an authorized client replaying calls.

### Revocation

Without shared state or an admin API, immediate per-token revocation is not guaranteed. Use short lifetimes, key rotation, and (planned) Git-managed deny policies rolled out to replicas. A rolling deploy can briefly run mixed policy versions; recording the config version (planned) diagnoses that window. Offboarding therefore takes effect when the person's last token expires: disable them at the issuer or IdP and rely on the configured maximum token lifetime (`max_credential_ttl_secs`; longer-lived credentials are rejected), not on revocation.

**Renewal.** Short lifetimes need automatic renewal, or every developer re-mints tokens by hand. Claude Code's [`apiKeyHelper`](https://code.claude.com/docs/en/llm-gateway-connect#rotate-credentials-with-apikeyhelper) runs a command that returns the credential and re-runs it periodically, so a helper that asks the team's issuer (for example, authenticated by OIDC) for a fresh short-lived token keeps renewal outside Wye. CI jobs use their workload identity directly, through an [identity-provider issuer](#gateway-credentials). Wye only verifies. Worked examples (GitHub Actions, developers): [operations.md](operations.md#credential-renewal).

### Caller-credential passthrough

A developer on a claude.ai subscription authenticates with their own login, not an API key. Claude Code supports this behind a gateway when only `ANTHROPIC_BASE_URL` is set: the login stays the credential, and the gateway must forward it together with the OAuth capability in `anthropic-beta`. Wye supports this as an explicit mode, `forward_caller_credential = true`:

- Loopback bind only; startup fails when it is combined with a non-loopback listener.
- No gateway credential, so no identity, model permissions, or provider-held budgets; usage bills to the caller's subscription.
- No per-request ceilings beyond the [local resource bounds](#local-resource-bounds): output-token ceilings are configured in `[auth]`, which this mode excludes, and the cost ceiling would not apply anyway, because subscription usage is not billed per token.
- Telemetry still works, attributed by `x-claude-code-session-id` and the agent headers. [`examples/subscription.toml`](../examples/subscription.toml) is a complete config.

## Per-request ceilings

Admission rejects requests that violate model allowances, payload or token bounds, or (planned, M2) a **conservatively computable** monetary ceiling.

**Implemented: output-token ceilings.** `[auth] max_output_tokens` and a credential's `max_output_tokens` claim (`wye token issue --max-output-tokens 8000`) bound the `max_tokens` a `/v1/messages` request may ask for, the `max_output_tokens` of a `/v1/responses` request, and the `max_completion_tokens` (or older `max_tokens`) of a `/v1/chat/completions` request, where a body naming both is refused; the stricter applies. On Responses the field is optional and its absence means the model's maximum, so under a ceiling a request without it is refused (`max_output_tokens is required`), before the provider sees a complete request; Codex does not set it, so give Codex users a credential without a ceiling. It caps output tokens per request; it is not a dollar cap.

- Read with the same scanner and hold as [model permissions](#gateway-credentials), never rewritten.
- Above the ceiling: `403 permission_error` before upstream contact when read during the hold; otherwise the upstream request is aborted mid-body (the provider never sees it whole) and the answer is still `403`.
- Not a non-negative integer, or repeated: `400`. Absent: forwarded (the provider requires it). `count_tokens` generates nothing and is not bounded.

A hard cost ceiling requires conservative bounds on every billable dimension: input tokens, output tokens (including hidden reasoning tokens), provider-hosted tools, service tiers, and any other usage charge. Tokenization and billing differ by provider; `max_output_tokens` is not a dollar cap. If a safe bound cannot be established, reject in hard-cap mode or label enforcement **estimated-only**. Never advertise estimation as a guaranteed ceiling.

### Provable cost ceiling

Wye can bound cost without a tokenizer, and therefore without per-provider tokenizer dependencies or network calls:

```text
max_input_tokens = min(input_bytes, model_context_window) + provider_overhead
max_cost = max_input_tokens × highest_input_price_reachable
         + output_cap       × output_price_per_token
```

The context window caps the input bound: a request larger than the window is rejected by the provider, so it cannot bill more input than the window holds.

- **Input bound.** For byte-level BPE tokenizers, each token covers at least one byte, so input tokens ≤ UTF-8 bytes of the billable text. Wye takes the request body size, an over-count that includes JSON syntax, so the bound stays conservative. Whether the property holds must be verified per provider tokenizer ([research.md](research.md#verification-needed)); a model whose tokenizer is not verified cannot use hard mode.
- **Output bound.** Wye inserts or lowers the output-token field ([allowlisted rewrite](architecture.md#forwarding-contract)) to `output_cap`, which must count reasoning tokens for the model. Where a provider does not bill reasoning within that limit, hard mode is unavailable for the model.
- **Images.** Claude Code sends images whenever it reads a screenshot or image file. Anthropic bills an image by its pixel dimensions, capped after server-side resizing, so each image has a fixed per-model token maximum; add that maximum per image block instead of counting its base64 bytes. Until that is verified, hard mode rejects images explicitly.
- **Everything else rejects in hard mode:** audio and files (priced by separate formulas, to be added per provider only when bounded), provider-hosted tools billed per call, priority or other service tiers, and any field the pricing table does not cover.
- Cached input is billed at or below the normal input price, so the bound ignores caching safely. Cache *writes* priced above normal input (Anthropic) use the highest write price the request can trigger; a 1-hour TTL costs more than a 5-minute one.
- **Prompt-length price tiers.** Where a provider raises the per-token price above a context-length threshold, price the whole bound at the tier its worst-case token count (input bytes plus output cap) reaches.
- **Provider-added input.** Some billed tokens are absent from the body: chat-template tokens, and the tool-use system prompt a provider adds when `tools` is present. Add each documented overhead to the bound; a model with undocumented overhead cannot use hard mode.

The bound is deliberately loose. It is a guarantee, not an estimate. Telemetry reports both the bound and the actual usage-derived cost.

**It is a runaway guard, not a budget.** Agent traffic such as Claude Code reads most input from the prompt cache at roughly a tenth of the input price, while the bound must assume cache writes; the bound can exceed actual cost by an order of magnitude. Use it to stop pathological requests (an enormous context at the most expensive tier, an unbounded output), and use [provider-held budgets](#provider-held-budgets) for spend. Hard mode is off by default on the Claude Code route; when enabled, it must not lower `max_tokens` below what the client requested unless the operator accepts truncated responses, and it rejects instead when the request's own `max_tokens` already exceeds the configured cap.

### Pricing metadata

**Implemented (M2).** Prices are declarative, versioned, and live in `wye.toml` next to the policy that uses them, so a price change is a reviewed commit like any other. Wye ships no built-in price table: one would be stale the day a provider changes prices, silently.

```toml
[pricing]
version = "anthropic-2026-10-06"     # required; recorded with every cost

# USD per million tokens, as decimal strings. The first entry whose
# pattern matches the model the provider reported wins.
[[pricing.models]]
models = ["claude-opus-5-5"]         # same patterns as [auth] models
input = "4.00"
output = "20.00"
cache_read = "0.20"
cache_write_5m = "5.00"              # 1.25 x input
cache_write_1h = "8.00"              # 2 x input

[[pricing.models]]
models = ["claude-sonnet-5-5", "claude-sonnet-5"]
input = "2.00"
output = "10.00"
cache_read = "0.20"
cache_write_5m = "2.50"
cache_write_1h = "4.00"

[[pricing.models]]
models = ["claude-haiku-5-5"]
input = "0.10"
output = "0.50"
# cache_read, cache_write_5m, cache_write_1h: copy from the provider's
# pricing page; until set, requests that use the cache cost "partial".
[[pricing.models.tiers]]             # prompt-length tier
above_prompt_tokens = 100000         # the whole request is priced at this tier
input = "0.50"
output = "2.50"
```

Rules:

- **Fields.** `input` and `output` are required; `cache_read`, `cache_write_5m`, and `cache_write_1h` are optional. Unknown fields fail startup, like the rest of the config. A tier lists the prices that change above `above_prompt_tokens` (input plus cache reads and writes) and inherits the rest from its model; tiers are sorted by threshold, and the highest one the request reaches applies to the whole request.
- **Matching.** By the model the provider reports in the response, not the one requested (an alias or a routing rewrite cannot misprice). No match: cost `unknown`. Unknown pricing never defaults to $0.
- **Per upstream (implemented).** Claude through another provider ([M4](architecture.md#claude-across-providers-planned-m4)), or another account or region, can be billed at different prices and regional premiums. An entry names `upstreams = ["aws-east"]` (upstream ids from the route config; config refuses unknown ids); entries with `upstreams` are tried before those without for requests served there.
- **Money is never floating point.** Each price is parsed once at startup into integer pico-USD per token (`"0.10"` per million is 100,000 pico-USD per token); a price with more than six decimals, a negative one, or one that overflows is a config error. A request's cost is the sum of tokens × price with checked `u64` arithmetic (the range covers millions of dollars per request) and fails closed: an overflow marks the cost `unknown`. Floating point appears only where a number is displayed.
- **Cache writes by TTL.** Anthropic reports `usage.cache_creation.ephemeral_5m_input_tokens` and `ephemeral_1h_input_tokens`; the usage scanner reads both. When only the total `cache_creation_input_tokens` is reported, writes are priced at `cache_write_1h` when it is set, the higher price, so the reported cost does not undercount, and the cost is `partial`.
- **What it does not price.** Fast mode, priority or other service tiers, regional inference premiums, and server tools billed per use (web search) are reported in `usage` but not in the table; a request that used any of them (a positive count in `usage.server_tool_use`, a `service_tier` or `speed` other than `standard`) is `partial`, with the token cost still recorded. A stream that ended before its final usage is `partial`. Batch requests do not pass through the gateway.
- **OpenAI Responses.** The same table prices `/v1/responses` by the reported model (entries are model patterns, so `gpt-5.5-*` and `claude-*` live side by side). OpenAI's `input_tokens` include cached tokens; Wye records the cached share (`input_tokens_details.cached_tokens`) as `cache_read_input_tokens` and the rest as `input_tokens`, so `cache_read` prices the cached share and records mean the same as on Messages. Reasoning tokens are part of `output_tokens`, as OpenAI bills them. Hosted tools billed per call (`web_search`, `file_search`, `code_interpreter`, `image_generation`) and a `service_tier` other than `default` make the cost `partial`. A stream cut before its terminal event has no usage (OpenAI reports it only there), so its cost is `unknown`.
- **No usage, no cost.** A record without observed usage (a gateway or provider error, a non-streaming response) is `unknown`, never $0.

With `[pricing]`, every usage record carries `cost_status` (`complete` | `partial` | `unknown`) and `pricing_version`, and `cost_usd` (an exact decimal string) when a cost was computed. Cost is computed in the usage writer task, off the request path. OTLP logs carry the same three as `wye.cost_usd`, `wye.cost_status`, `wye.pricing_version`; the `wye.cost` metric sums priced costs by model and request class (in USD, a double: the export is display); `wye report` adds a cost column, summed exactly, marked `+` when it includes partial or unknown costs. The same table feeds the [provable cost ceiling](#provable-cost-ceiling), which additionally needs each model's context window and verified bounds.

## State boundary

No external store and no coordination are intentional constraints. Therefore Wye cannot enforce exact cumulative quotas across replicas.

| Control | Built into Wye? | Reason |
|---|---|---|
| Signature, identity, expiry, audience | Yes | Request plus static public key |
| Model allowlist and static policy | Yes | Request plus immutable config |
| Per-request token limits | Yes, where the protocol gives enforceable bounds | Request-local |
| Per-request billed-cost ceiling | Conditional | Hard only when all billable charges are bounded |
| Global RPM / TPM | No | Needs accumulated consumption and coordination |
| Global concurrent streams | No | Needs coordinated permits |
| Team or monthly budget | Not in Wye; delegated to the [provider](#provider-held-budgets) | Needs an authoritative ledger; the provider already has one |
| Single-use credential redemption | No | Needs replay tracking |
| Historical usage and cost charts | Via external telemetry | Observation, not enforcement |
| Provider-side quotas | External | Wye does not depend on them for correctness |

Rate limiting is stateful even when the state is ephemeral, so a local token bucket is excluded. External ingress or application rate limiting is a deployment responsibility.

### Local resource bounds

Per-process limits that protect the process are allowed: maximum connections, maximum request body size, maximum total buffered request bytes, and bounded telemetry queues. They answer "can this process serve another request now", not "has this caller used too much", so they are not quotas. Exceeding one rejects with an overload error (Anthropic `overloaded_error`, status `529`, on `/v1/messages`; status `503` in OpenAI's error shape on `/v1/responses`; both with `retry-after: 1` and `x-should-retry: true`) so clients back off and retry; it never queues without bound. Limits are fixed in config and never shared between replicas. Implemented: a 32 KiB read buffer per client connection (which also bounds request headers; larger headers get 431), `server.max_request_body_bytes`, `server.max_held_request_bytes` (request bytes held for model permissions and output-token ceilings, across all requests), `server.max_concurrent_requests` (default 10,000 proxied requests in flight; a request holds its permit until its response body ends), `server.client_write_timeout_secs` (default 60: a client that accepts no response bytes that long is disconnected, releasing its permit and buffers), and the bounded usage-log and OTLP queues ([operations.md](operations.md#telemetry)).

### Provider-held budgets

Providers already keep an authoritative per-account spend ledger. Wye can use it instead of building one:

- Create one provider project or workspace per budget owner (team, project, environment), each with its own API key and a spend limit set at the provider.
- Map gateway identities to provider credentials in config: a credential's `sub` (or a `budget` claim) selects which provider key Wye uses.
- When the provider limit is exhausted, the provider rejects requests; Wye relays that as an explicit budget error.

This gives cumulative budgets enforced by the party that bills, with no Wye state and no coordination between replicas. Limits are as strict as the provider makes them: some providers enforce spend limits as hard stops, others only alert, and enforcement may lag actual usage. Verify each provider before relying on it ([research.md](research.md#verification-needed)). Combined with the [provable per-request ceiling](#provable-cost-ceiling), overshoot past the provider limit is bounded by the in-flight requests at the moment of cutoff.

Prompt caches are per account, so splitting traffic across more provider keys reduces cache sharing between budget owners. That is usually the intended isolation.

### Why signed balances fail

If a token says `$20 remaining` and Wye returns a new `$18` token, a client can still reuse the original `$20` token on another replica. Attenuating a `$10` capability into two `$8` capabilities permits two valid spends. Signature verification does not track redemption. This is the double-spending boundary known from Privacy Pass, offline e-cash, and capability research.

### Delayed cutoff is external and optional

```text
Wye OTLP usage ──> aggregator ──> threshold detection
                                         │
                                         v
                          optional GitOps policy change
                                         │
                                         v
                              new config rollout
```

This gives monitoring and delayed cutoff, not a budget. Export loss, aggregation windows, rollout delay, in-flight requests, and billing uncertainty cause overshoot.

If an **externally enforced** aggregate rate is at most `R` requests/s, each request's true maximum charge is `C`, and the stop delay is at most `T` seconds, additional spend in that interval is bounded by `R × C × T`. Wye does not guarantee `R`, so this is not a built-in spending bound.

### Rejected approaches

- **Redis / PostgreSQL / SQLite ledgers:** solve global quotas but add state and an operational dependency; SQLite makes even one instance stateful.
- **Peer-to-peer actors:** ownership, replication, fencing, quorum, and failover keep state inside the gateway; total cluster loss destroys unpersisted balances.
- **Escrow / quota leasing:** reduces coordination but still needs durable ownership and recovery.
- **Gossip / CRDT counters:** converge eventually but cannot cap concurrent overspend without coordination.
- **RLN / nullifiers, one-time credentials, hash chains, proof of work:** prevention or detection of double spending needs remembered redemption state or a trusted counter.
- **Trusted hardware:** could hold monotonic state client-side but is impractical as a default for Codex or generic applications.

## Security requirements

- Provider API keys never leave the gateway or its deployment environment.
- Gateway credentials are validated before dispatch; scoped and short-lived where practical.
- Untrusted browsers never receive reusable provider or gateway credentials.
- Upstream URLs come only from trusted config.
- Bound inbound headers, JSON bodies, parsed SSE event sizes, outbound buffering, redirects, and timeouts.
- Protect key material; prefer public-key verification so signing keys stay out of the data plane.
- Errors and telemetry never leak prompts or secrets by default.
- Public applications supply their own abuse prevention and global limits.
- Insecure local-development auth settings never apply to a non-loopback bind.

No exactly-once delivery, exactly-once billing, or durable execution is guaranteed.
