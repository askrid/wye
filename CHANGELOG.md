# Changelog

## 0.1.0

First public release. Wye is a working prototype: tested and benchmarked on
one machine, used with Claude Code through a claude.ai subscription. Expect
the config format to change before 1.0.

What is in it:

- Anthropic Messages (`/v1/messages`, `count_tokens`), OpenAI Responses, and
  OpenAI Chat Completions, forwarded byte for byte, streams relayed event by
  event.
- Routes with several accounts. Each Claude Code session stays on one account
  (weighted rendezvous hashing on the session id). Failover on `429`, `529`,
  or a connection refused or cut before the request body was delivered, and
  never after the first response byte.
- Providers for Claude: Anthropic, Claude Platform on AWS, and Amazon Bedrock,
  with API keys.
- Gateway credentials: Wye's own Ed25519 tokens, or identity-provider tokens
  (RS256, ES256, EdDSA) with keys pinned in config. Model permissions and
  output-token ceilings, which a credential can narrow but never widen.
- Model aliases with per-subject rollout, and upgrade checks that report
  which requests a target model would reject.
- Usage records per request (tokens, cost, cache hits and misses, session,
  subagent, request class, tool calls and time), OTLP logs, traces, and
  metrics, and optional cache-miss diagnostics.
- Cost from a price table in config, in exact integer arithmetic.
- Commands: `init`, `serve`, `check-config`, `doctor`, `explain`,
  `policy-test`, `policy-diff`, `report`, `probe`, `claude`, `codex`, and
  `token`. `wye claude` turns on Claude Code's gateway hints (request
  class, prompt id, tool times), and `wye claude --bg` starts background
  sessions that go through the gateway too.
- Release archives for Linux and macOS (x86_64, arm64) with checksums and
  build provenance attestations, and service definitions for a user
  LaunchAgent (macOS) and systemd unit (Linux) in docs/operations.md.

Not yet checked against the real services:

- Bedrock and Claude Platform on AWS were built from the providers'
  documentation and tested against mocks. Bedrock's API key header in
  particular is unconfirmed (`x-api-key` is sent).
- Codex support was built from Codex's source and tests, not from a real
  session.

Known limits:

- No Windows build, no published container image, no Vertex AI, no AWS SigV4.
- Provider keys are read at startup; short-lived keys need a restart to
  refresh.
- No cumulative budgets or rate limits, by design: those stay with the
  provider.
