# Security and policy

What Wye authorizes and enforces per request, what it cannot enforce without shared state, and its security requirements.

## Gateway tokens

[Provider keys](glossary.md#requests-and-where-they-go) authenticate Wye to providers; they do not authenticate callers to Wye. Callers present signed [gateway tokens](glossary.md#access). Without an `[auth]` section Wye listens on loopback only; with one it may listen on any address, and every request to an API endpoint must carry a valid gateway token.

Claims of Wye tokens (identity-provider tokens are below):

```json
{
  "iss": "wye",
  "sub": "team-research",
  "aud": "wye-prod",
  "iat": 1791996400,
  "exp": 1792000000,
  "models": ["claude-sonnet-*", "claude-haiku-*"],
  "max_output_tokens": 4096
}
```

The issuer signs tokens outside the runtime, with `wye token issue` (offline, from a private key file) or an external issuer producing the same format. Replicas hold verification keys only, selected by key ID so keys can rotate. All checks run before contacting upstream.

Tokens are compact JWTs, parsed by one strict parser whatever their issuer.

- Refused with `401 authentication_error` before any upstream contact: another `alg`, an unknown header field (including `crit`), a `kid` not in `[[auth.signing_keys]]` or an issuer's key set, a bad signature, the wrong `iss` or `aud`, expiry, `iat`/`nbf` more than 60 s ahead, or `exp - iat` above `max_gateway_token_ttl_secs` (default one week). Claims are parsed only after the signature verifies.
- Read from `Authorization: Bearer` or `x-api-key`; replaced by the provider key, never forwarded.
- Cached per connection: a request repeating the last verified token skips signature verification, compares the bytes in constant time, and rechecks expiry. The cache holds one verified token and dies with the connection; keys are fixed for the process lifetime, so its answers equal a fresh check.
- On a reused connection, authenticated requests have the same overhead as unauthenticated ones; a full verification's: `auth_` in [benchmarks.md](benchmarks.md#current-harness-and-baseline).

The key fixes the algorithm; the token never chooses it: Ed25519 for Wye tokens, one configured algorithm per identity provider. General JWT algorithm negotiation (`alg: none`, HMAC/RSA confusion) is a well-known source of verification bugs. A pinned `alg` removes that class of bug, and ordinary JWT tooling can still read the encoding.

Model permissions: `[auth] models` lists the models any token may use; a token's `models` claim (`wye token issue --models claude-sonnet-*,claude-haiku-*`) narrows it, never widens it. An entry is an exact name or a prefix ending in `*`, which covers dated ids and Claude Code's background Haiku model. With [model aliases](architecture.md#model-aliases-and-upgrade-checks), permissions apply to the [served model](glossary.md#models).

- **[The hold](glossary.md#requests-and-where-they-go)**: with either list, Wye holds the request body only until its top-level `model` is read (Claude Code sends it first, so usually one chunk). Without either list (or an alias, an output-token ceiling, or a provider with its own model ids), the body is not read.
  - `server.max_total_held_bytes` bounds held bytes across all requests together (default 64 MiB), each chunk counting at least 1 KiB for the memory it pins. Beyond it: an overload error, or `413` when the request's own chunks alone would pass it, since no retry could succeed.
  - A client that stalls while held gets `408`, and its held bytes are released ([limits](operations.md#failure-behavior)).
- **Refusals**, before upstream contact:
  - A forbidden model gets `403 permission_error`, naming the model as the client sent it (an alias, not what it maps to).
  - A missing, non-string, escaped, or repeated `model`, an escaped or non-ASCII top-level key, or a body that is not a JSON object gets `400 invalid_request_error`.
  - A compressed body or a charset other than UTF-8 gets `415` (on any `Content-Encoding` or `Content-Type` line, and two `Content-Type` lines too), since a provider that decoded it could find other keys in the same bytes.
- **Strict JSON**: the body is read as strict UTF-8 JSON, the way a conforming provider reads it. A byte outside JSON's grammar (the NUL bytes of UTF-16 text, an unquoted key, a comment, a second value after the object) is refused rather than skipped, and top-level keys compare ignoring ASCII case (some decoders bind `MODEL` to `model`).
- **Scope**: permissions read the top-level `model`. A model named inside the body, such as the `model` of a Responses `image_generation` tool, is not checked.
- **The tail check**: JSON allows a repeated key, and a provider might honor a later `"model"`, so the rest of the body is lexed as it streams; a second top-level `model`, or an escaped top-level key, fails the body and aborts the upstream request. It adds CPU overhead per MB on structure-dense bodies ([benchmarks.md](benchmarks.md#current-harness-and-baseline)) and can go if the provider is verified to reject duplicate keys.

When the token and the config (`[auth]`) both set a limit, the stricter value applies: a token can narrow configured policy but never widen it.

Identity-provider tokens: Wye also accepts JWTs issued directly by a trusted OIDC identity provider, so teams need no token-minting service: GitHub Actions or another workload identity for CI, and the team's IdP (through `apiKeyHelper`) for developers.

Each `[[auth.identity_providers]]` entry pins `issuer`, `audience`, one `algorithm` (`RS256`, `ES256`, or `EdDSA`; never HMAC, never read from the token), and `jwks_file`, the identity provider's published JSON Web Key Set committed next to the config and read at startup (keys of other types, curves, algorithms, or marked for encryption are skipped). The `issuer`, `audience`, and `[[auth.signing_keys]]` of Wye tokens become optional: a deployment can trust only its IdP.

- **Key selection**: a token's `kid` selects its key, and every key belongs to one issuer; a `kid` used by two issuers, or by an issuer and `[[auth.signing_keys]]`, fails startup. The header must name that issuer's algorithm, and after the signature verifies the claims must name that issuer, so claims are still parsed only after the signature. An identity provider's token headers may carry extra fields (`typ`, `x5t`), which are ignored; `crit` is refused.
- **Claims**: `iss`, `aud`, `exp`, and `iat` are required, with the same rules as Wye tokens (`max_gateway_token_ttl_secs`, 60 s skew for `iat`/`nbf`). `subject_claim` (default `sub`; for example `email`, or `repository` for GitHub Actions) names the [subject](glossary.md#access) in the usage log and telemetry; a token without it is refused. An identity provider's `models` or `max_output_tokens` claims are ignored: limits come from config.
- **Grants**: `[[auth.identity_providers.grants]] claim = "groups", value = "ml-team", models = ["claude-sonnet-*"]` grants models to tokens whose claim equals the value or is a list containing it. A token gets the union of its matching grants, still within `[auth] models`. With grants configured, a token matching none may use no model (`403 permission_error`); an issuer without grants leaves `[auth] models` in charge.
- **Tradeoffs**: RSA and ECDSA verification and a generic claims object are a larger surface than the fixed Ed25519 format, so the algorithm is pinned per issuer and keys come only from config; the per-connection cache applies as to Wye tokens.
- **Key rotation**: pinned keys need a config change when the IdP rotates them: commit the new set before the IdP starts signing with it (identity providers publish upcoming keys ahead of use). Fetching the JWKS at runtime would remove that step but adds network I/O and cached state; it stays a later, explicit option.

A token can assert a maximum per request, never the cumulative amount spent; deriving a narrower token without the issuer is research ([narrowable tokens](research.md#3-narrowable-tokens-for-agent-swarms)). Short TTLs reduce exposure but do not make tokens single-use. Proof of possession helps against theft but not against an authorized client replaying calls.

### Revocation

Without shared state or an admin API, immediate per-token revocation is not guaranteed. Use short lifetimes and key rotation; a deny list in config is [planned](roadmap.md#m1-team-ready-claude-code). A rolling deploy can briefly run mixed config versions; recording the config version in usage records ([planned](roadmap.md#m1b-agent-aware-visibility)) would show that window. Offboarding therefore takes effect when the person's last token expires: disable them at the issuer or IdP and rely on the configured maximum token lifetime (`max_gateway_token_ttl_secs`; longer-lived tokens are refused), not on revocation.

Short lifetimes need automatic renewal: [token renewal](operations.md#token-renewal). Wye only verifies.

### Caller-credential passthrough

`forward_caller_credential = true`, which `wye init` writes by default, is the passthrough setup, for one developer: each caller's own credential goes upstream, whether Claude Code's claude.ai login or an API key. Claude Code supports a login behind a gateway when only `ANTHROPIC_BASE_URL` is set; Wye forwards it together with the OAuth capability in `anthropic-beta`.

- Loopback bind only; startup fails when it is combined with a non-loopback listener.
- No gateway token, so no identity or model permissions. A login's usage counts against the caller's plan; an API key's bills the API.
- No per-request ceilings beyond the [local resource bounds](#local-resource-bounds): output-token ceilings are configured in `[auth]`, which passthrough excludes, and a cost ceiling would not apply to a plan, which is not billed per token.
- Usage records and telemetry are still attributed by `x-claude-code-session-id` and the agent headers. [`examples/developer/wye.toml`](../examples/developer/wye.toml) is a complete config.

## Per-request ceilings

Admission refuses requests that violate model permissions, payload or token bounds, or (planned) a monetary ceiling that can be computed conservatively.

Output-token ceilings are implemented. `[auth] max_output_tokens` and a token's `max_output_tokens` claim (`wye token issue --max-output-tokens 8000`) bound the `max_tokens` a `/v1/messages` request may ask for, the `max_output_tokens` of a `/v1/responses` request, and the `max_completion_tokens` (or older `max_tokens`) of a `/v1/chat/completions` request, where a body naming both is refused; the stricter applies. On Responses the field is optional and its absence means the model's maximum, so under a ceiling a request without it is refused (`max_output_tokens is required`), before the provider sees a complete request; Codex does not set it, so give Codex users a token without a ceiling.

- Read with the same scanner and hold as [model permissions](#gateway-tokens), never rewritten, and only when a ceiling applies: without one, a value such as `null` or `4096.0` is the provider's to judge.
- Above the ceiling: `403 permission_error` before upstream contact when read during the hold; otherwise the upstream request is aborted mid-body (the provider never sees it whole) and the answer is still `403`.
- Not a non-negative integer, or repeated: `400`. Absent: forwarded on Messages (the provider requires it), refused on the OpenAI APIs. `count_tokens` generates nothing and is not bounded.
- The ceiling bounds one generation. Chat Completions' `n` asks for several choices, each up to the ceiling, so a request may generate `n` times as much; `n` is not read yet.

### Provable cost ceiling

Planned ([roadmap](roadmap.md#m2-cost-policy-for-claude)), not built. `max_output_tokens` is not a dollar cap: a hard cost ceiling needs a conservative bound on every billable dimension (input tokens, output tokens including hidden reasoning tokens, provider-hosted tools, service tiers, any other usage charge), and tokenization and billing differ by provider. Where Wye cannot establish a safe bound, hard mode refuses the request or labels enforcement estimated-only; an estimate is never advertised as a guaranteed ceiling. The bound and what it needs verified: [research.md](research.md#6-provable-cost-bound).

### Price table

Prices are declarative, versioned, and live in `wye.toml` next to the policy that uses them, so a price change is a reviewed commit like any other. Wye ships no built-in price table, which would go silently stale the day a provider changed its prices.

```toml
[pricing]
version = "anthropic-2026-11-01"     # required; recorded with every cost

# USD per million tokens, as decimal strings. Entries scoped to the
# serving upstream (`upstreams = [...]`) are tried first; among the rest,
# the first whose pattern matches the model the provider reported wins.
[[pricing.prices]]
models = ["claude-opus-5-5"]         # same patterns as [auth] models
input = "4.00"
output = "20.00"
cache_read = "0.20"                  # 0.05 x input here; 0.1 x on most models
cache_write_5m = "5.00"              # 1.25 x input
cache_write_1h = "8.00"              # 2 x input

[[pricing.prices]]
models = ["claude-sonnet-5-5"]
input = "2.00"
output = "10.00"
cache_read = "0.10"
cache_write_5m = "2.50"
cache_write_1h = "4.00"

[[pricing.prices]]
models = ["claude-sonnet-5"]
input = "2.00"
output = "10.00"
cache_read = "0.20"
cache_write_5m = "2.50"
cache_write_1h = "4.00"

[[pricing.prices]]
models = ["claude-haiku-5-5"]
input = "0.10"
output = "0.50"
# cache_read, cache_write_5m, cache_write_1h: copy from the provider's
# pricing page; until set, requests that use the cache cost "partial".
[[pricing.prices.tiers]]             # prompt-length tier
above_prompt_tokens = 100000         # the whole request is priced at this tier
input = "0.50"
output = "2.50"
```

Which price applies:

- **Fields**: `input` and `output` are required; `cache_read`, `cache_write_5m`, and `cache_write_1h` are optional. Unknown fields fail startup, like the rest of the config.
- **Tiers**: a tier lists the prices that change above `above_prompt_tokens` (input plus cache reads and writes) and inherits the rest from its model; tiers are sorted by threshold, and the highest one the request reaches applies to the whole request.
- **Matching**: by the model the provider reports in the response, not the one requested (an alias or a routing rewrite cannot misprice). No match: cost `unknown`. Unknown pricing never defaults to $0.
- **Per upstream**: Claude through another provider ([across providers](architecture.md#claude-across-providers)), or another account or region, can be billed at different prices and regional premiums. An entry names `upstreams = ["aws-east"]` ([upstream](glossary.md#requests-and-where-they-go) ids from the route config; config refuses unknown ids); entries with `upstreams` are tried before those without for requests served there.

How cost is computed:

- **Integer money**: money is never floating point. Each price is parsed once at startup into integer pico-USD per token (`"0.10"` per million is 100,000 pico-USD per token); a price with more than six decimals, a negative one, or one that overflows is a config error.
- **Checked arithmetic**: a request's cost is the sum of tokens × price with checked `u64` arithmetic (the range covers millions of dollars per request) and fails closed: an overflow marks the cost `unknown`. Floating point appears only where a number is displayed.
- **Cache writes by TTL**: Anthropic reports `usage.cache_creation.ephemeral_5m_input_tokens` and `ephemeral_1h_input_tokens`; the usage scanner reads both. Cache writes the split does not cover (no split reported, or a total `cache_creation_input_tokens` larger than it) are priced at `cache_write_1h` when it is set, else `cache_write_5m`, so the reported cost does not undercount, and the cost is `partial`.

When cost is partial or unknown:

- **Not priced**: fast mode, priority or other service tiers, regional inference premiums, and server tools billed per use (web search) are reported in `usage` but not in the table. A request that used any of them (a positive count in `usage.server_tool_use`, a `service_tier` or `speed` other than `standard`) is `partial`, with the token cost still recorded. Batch requests do not pass through the gateway.
- **Cut streams**: a stream that ended before its final usage is `partial`.
- **No usage, no cost**: a record without observed usage (a gateway or provider error, `count_tokens`, a response that is neither SSE nor JSON) is `unknown`, never $0.

OpenAI Responses and Chat Completions:

- The same table prices `/v1/responses` and `/v1/chat/completions` by the reported model (entries are model patterns, so `gpt-5.5-*` and `claude-*` live side by side).
- OpenAI's `input_tokens` (Chat: `prompt_tokens`) include cached tokens; Wye records the cached share (`input_tokens_details.cached_tokens`, Chat: `prompt_tokens_details.cached_tokens`) as `cache_read_input_tokens` and the rest as `input_tokens`, so `cache_read` prices the cached share and records mean the same as on Messages. Reasoning tokens are part of `output_tokens`, as OpenAI bills them.
- Hosted tools billed per call (`web_search`, `file_search`, `code_interpreter`, `image_generation`) and a `service_tier` other than `default` make the cost `partial`. A stream cut before its terminal event has no usage (OpenAI reports it only there), so its cost is `unknown`.

With `[pricing]`, every usage record carries `cost_status` (`complete` | `partial` | `unknown`) and `pricing_version`, and `cost_usd` (an exact decimal string) when a cost was computed. Cost is computed in the usage writer task, off the request path. OTLP carries them too, and the `wye.cost` metric is in USD, a double: the export is display ([telemetry](operations.md#usage-records-and-telemetry)).

## State boundary

Wye has no external store and no coordination by design, so it cannot enforce exact cumulative quotas across replicas. The approaches weighed and rejected for them: [research.md](research.md#rejected-approaches).

| Control | Built into Wye? | Reason |
|---|---|---|
| Signature, identity, expiry, audience | Yes | Request plus static public key |
| Model permissions and static policy | Yes | Request plus immutable config |
| Per-request output-token ceilings | Yes, where the protocol gives enforceable bounds | Request-local |
| Per-request cost ceiling | Planned, not built ([provable cost ceiling](#provable-cost-ceiling)) | Hard only when all billable charges are bounded |
| Global RPM / TPM | No | Needs accumulated consumption and coordination |
| Global concurrent streams | No | Needs coordinated permits |
| Team or monthly budget | Not in Wye; delegated to the [provider](#provider-held-budgets) | Needs an authoritative ledger; the provider has one |
| Single-use gateway tokens | No | Needs replay tracking |
| Historical usage and cost charts | Via your [OpenTelemetry backend](glossary.md#usage-and-cost) | Observation, not enforcement |
| Provider-side quotas | External | Wye does not depend on them for correctness |

Rate limiting is stateful even when the state is ephemeral, so a local token bucket is excluded. External ingress or application rate limiting is a deployment responsibility.

### Local resource bounds

Per-process limits that protect the process are allowed. They answer "can this process serve another request now", not "has this caller used too much", so they are not quotas. Nothing queues without bound, and limits are never shared between replicas.

- `server.max_concurrent_requests` and `server.max_total_held_bytes`: beyond them, an overload error (Anthropic `overloaded_error`, status `529`, on the Messages endpoints; status `503` in OpenAI's error shape on the OpenAI endpoints; both with `retry-after: 1` and `x-should-retry: true`) so clients back off and retry. A request whose own held chunks alone would pass `max_total_held_bytes` gets `413` instead: a retry could not succeed. A request holds its permit until its response body ends.
- `server.max_total_kept_bytes`: request bytes kept so that [failover](glossary.md#requests-and-where-they-go) can resend them, across all requests; beyond it, a request is forwarded without failover, never refused. A route's `failover_max_body_bytes` bounds one request's kept body the same way.
- `server.max_request_body_bytes`: a body past it gets `413`.
- A 32 KiB read buffer per client connection, which also bounds request headers: larger headers get `431`.
- `server.client_write_timeout_secs`: a client that accepts no response bytes that long is disconnected, releasing its permit and buffers.
- The bounded usage-log and OTLP queues ([operations.md](operations.md#usage-records-and-telemetry)).

Defaults: [`examples/wye.toml`](../examples/wye.toml).

### Provider-held budgets

Planned next after [fewer expensive tokens](roadmap.md#m2c-fewer-expensive-tokens), not built: cumulative spend limits enforced by the provider that bills, which already keeps an authoritative ledger. Each budget owner's gateway tokens would reach only upstreams holding its own provider key, whose spend limit the provider enforces, so Wye keeps no ledger and replicas need no coordination ([design](roadmap.md#m2-cost-policy-for-claude)).

## Security requirements

- Provider keys never leave the gateway or its deployment environment.
- Gateway tokens are validated before dispatch; scoped and short-lived where practical.
- Untrusted browsers never receive reusable provider keys or gateway tokens.
- Upstream URLs come only from trusted config.
- Bound inbound headers, JSON bodies, parsed SSE event sizes, outbound buffering, redirects, and timeouts.
- Protect key material; prefer public-key verification so signing keys stay out of the data plane.
- Errors, usage records, and telemetry never leak prompts or secrets by default.
- Public applications supply their own abuse prevention and global limits.
- Insecure local-development auth settings never apply to a non-loopback bind.

No exactly-once delivery, exactly-once billing, or durable execution is guaranteed.
