# Security and policy

What Wye authorizes and enforces per request, what it cannot enforce without shared state, and its security requirements.

## Gateway credentials

Provider credentials authenticate Wye to providers; they do not authenticate callers to Wye. Callers present signed gateway credentials. Without an `[auth]` section Wye listens on loopback only; with one it may listen on any address, and every proxied request must carry a valid credential.

Illustrative claims:

```json
{
  "iss": "wye-credential-issuer",
  "sub": "team-research",
  "aud": "wye-prod",
  "exp": 1792000000,
  "models": ["fast", "coding"],
  "max_output_tokens": 4096,
  "max_request_cost_usd": "0.05"
}
```

The issuer signs credentials outside the runtime, with `wye token issue` (offline, from a private key file) or an external issuer producing the same format. Replicas hold verification keys only, selected by key ID so keys can rotate. Verify signature, issuer, audience, expiry, and bounds. Reject unknown critical claims. All checks run before contacting upstream. Accept the credential from `Authorization: Bearer` or `x-api-key`; Claude Code sends whichever credential variable the developer set.

**Implemented (M1 stage 4a):** compact JWTs with `alg` pinned to `EdDSA` (the "JWT-compatible encoding" option below, chosen so IdP-issued tokens can later share one strict parser). Any other `alg`, any unknown header field (including `crit`), a `kid` not in `[auth.keys]`, a bad signature, the wrong `iss` or `aud`, expiry, `iat`/`nbf` more than 60 s in the future, or `exp - iat` above `max_credential_ttl_secs` (default one week) rejects the request with `401 authentication_error` before any upstream contact; claims are parsed only after the signature verifies. Credentials are read from `Authorization: Bearer` or `x-api-key`; the gateway credential is replaced by the provider key and never forwarded. Each connection caches the last credential it verified: a request repeating it skips signature verification, compares the bytes in constant time, and rechecks expiry. The cache is per connection, holds only a verified credential, and dies with the connection, so nothing is shared and answers equal a fresh check (keys are fixed for the process lifetime). Cost on the development laptop: a full verification adds about 88 µs of CPU per request (the Ed25519 check itself is 55–62 µs); on a reused connection, authenticated requests cost the same CPU as unauthenticated ones (`auth_` benchmark metrics). 

**Implemented (M1 stage 4b): model permissions.** `[auth] models` lists the models any credential may use, and a credential's `models` claim (`wye token issue --models claude-sonnet-*,claude-haiku-*`) narrows it; both must allow the model, so a credential can never widen the configured list. An entry is an exact name or a prefix ending in `*`, which covers dated model ids and the background Haiku model Claude Code uses. Without either list, nothing is checked and the body is not read. With one, Wye reads the request body only until its top-level `model` value has been read, holding those bytes (Claude Code sends `model` first, so usually only the first chunk waits). A forbidden model gets `403 permission_error`; a missing, non-string, escaped, or repeated `model`, an escaped top-level key, or a body that is not a JSON object gets `400 invalid_request_error`. All of this happens before any upstream contact. Held bytes count against `server.max_held_request_bytes` (default 64 MiB) across all requests, and beyond it requests get `529 overloaded_error`.

JSON allows a repeated key, and a provider might honor a later `"model"` than the one checked, so the rest of the body is still lexed as it streams. A second top-level `model` key, or a top-level key written with escapes, fails the body mid-stream: the upstream request is aborted and never completes. That tail check costs CPU per uploaded MB on structure-dense bodies ([research.md](research.md#current-harness-and-baseline)). If the provider is verified to reject duplicate keys, the check can be dropped.

Use one fixed algorithm, Ed25519, determined by the key rather than by a header field in the token. General JWT algorithm negotiation (`alg: none`, HMAC/RSA confusion) is a well-known source of verification bugs; a fixed format removes that class entirely. A JWT-compatible encoding with `alg` pinned to `EdDSA` is acceptable if ecosystem tooling matters more.

**Limit precedence.** When the credential and `[policy]` both set a limit, the stricter value applies. A credential can narrow configured policy, never widen it.

**Identity-provider tokens.** Wye can also accept JWTs issued directly by a trusted OIDC identity provider, so teams need no token-minting service: GitHub Actions or another workload identity for CI, and the team's IdP (through `apiKeyHelper`) for developers. Each `[[auth.issuers]]` entry pins the issuer, audience, one algorithm, and its signing keys in config; claims (for example `sub` or a groups claim) map to model permissions and limits in config. Verification stays request-local and stateless. Tradeoffs: JWT parsing is a larger attack surface than the fixed Ed25519 format, so the algorithm is pinned per issuer and never read from the token header; pinned keys need a config change when the IdP rotates them (fetching JWKS at runtime is a later, explicit option, cached in memory only).

Macaroons or Biscuit allow attenuation (team → project → subagent) without contacting an issuer; not needed for the MVP, and tracked as [delegable credentials](research.md#3-delegable-credentials-for-agent-swarms).

A token can assert a **maximum per request**, never the cumulative amount spent. Short TTLs reduce exposure but do not make credentials single-use. Proof of possession helps against theft but not against an authorized client replaying calls.

### Revocation

Without shared state or an admin API, immediate per-token revocation is not guaranteed. Use short lifetimes, key rotation, and Git-managed deny policies rolled out to replicas. A rolling deploy can briefly run mixed policy versions; record the config version to diagnose that window. Offboarding therefore takes effect when the person's last token expires: disable them at the issuer or IdP and rely on a configured maximum token lifetime (`max_credential_ttl`, rejected above it), not on revocation.

**Renewal.** Short lifetimes need automatic renewal, or every developer re-mints tokens by hand. Claude Code's [`apiKeyHelper`](https://code.claude.com/docs/en/llm-gateway-connect#rotate-credentials-with-apikeyhelper) runs a command that returns the credential and re-runs it periodically, so a helper that asks the team's issuer (for example, authenticated by OIDC) for a fresh short-lived token keeps renewal outside Wye. CI jobs mint tokens from their workload identity the same way. Wye only verifies.

### Caller-credential passthrough

A developer on a claude.ai subscription authenticates with their own login, not an API key. Claude Code supports this behind a gateway when only `ANTHROPIC_BASE_URL` is set: the login stays the credential, and the gateway must forward it together with the OAuth capability in `anthropic-beta`. Wye supports this as an explicit mode, `forward_caller_credential = true`:

- Loopback bind only; startup fails when it is combined with a non-loopback listener.
- No gateway credential, so no identity, model permissions, or provider-held budgets; usage bills to the caller's subscription.
- Per-request token bounds still apply. The cost ceiling does not, because subscription usage is not billed per token.
- Telemetry still works, attributed by `x-claude-code-session-id`.

## Per-request ceilings

Admission rejects requests that violate model allowances, payload or token bounds, or a **conservatively computable** monetary ceiling.

**Implemented: output-token ceilings.** `[auth] max_output_tokens` and a credential's `max_output_tokens` claim (`wye token issue --max-output-tokens 8000`) bound the `max_tokens` a `/v1/messages` request may ask for; the stricter one applies, so a credential only lowers the configured ceiling. Wye reads the top-level `max_tokens` with the same scanner and hold as [model permissions](#gateway-credentials) and does not rewrite it: a value above the ceiling gets `403 permission_error` before any upstream contact when it was read during the hold, and otherwise aborts the upstream request mid-body (the provider never sees a complete request) and still answers `403`. A `max_tokens` that is not a non-negative integer, or appears twice, gets `400`; a request without it is forwarded, since the provider rejects it anyway. `count_tokens` generates nothing and is not bounded. This caps output tokens per request; it is not a dollar cap (see below).

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

Pricing is declarative and versioned. Estimates carry a pricing-version identifier and distinguish cached-input pricing where supported. Unknown pricing never defaults to $0. Routing may use static prices and request features but must not infer task quality from per-token price alone.

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

Per-process limits that protect the process are allowed: maximum connections, maximum request body size, maximum total buffered request bytes, and bounded telemetry queues. They answer "can this process serve another request now", not "has this caller used too much", so they are not quotas. Exceeding one rejects with an overload error (Anthropic `overloaded_error`, status `529`, on `/v1/messages`) so clients back off; it never queues without bound. Limits are fixed in config and never shared between replicas. Implemented: a 32 KiB read buffer per client connection (which also bounds request headers; larger headers get 431), `server.max_request_body_bytes`, and `server.max_concurrent_requests` (default 10,000 proxied requests in flight; a request holds its permit until its response body ends).

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
