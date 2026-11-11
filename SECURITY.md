# Security

Report a vulnerability privately at [github.com/askrid/wye/security/advisories/new](https://github.com/askrid/wye/security/advisories/new), not in a public issue.

## What a running gateway holds and reaches

- Secrets: the provider keys and OTLP headers its config names, read from environment variables or files at startup. The passthrough setup holds none. `wye serve` never loads the signing key that issues Wye tokens.
- Disk: `wye serve` writes only the usage log named by `--usage-log` (`wye service` sets `~/.wye/usage.jsonl`), mode 600 when it creates it, and its rotated files; without the flag, usage records go to stdout and it writes nothing.
- Network: `wye serve` connects only to its configured upstreams and, with `[telemetry]`, its OTLP endpoint. No update check, no telemetry home, no crash reporting; identity providers' keys are read from files, never fetched. A request cannot choose the host. Four API paths are forwarded, to the base URLs in config.
- Endpoints: those four paths, `/healthz`, `/readyz`, `HEAD /api/hello` (an empty `200` for Claude Code's warm-up probe, no upstream), and `GET /v1/models` with `[discovery]`. No admin API, UI, or database.
- Gateway tokens: each issuer's algorithm is fixed in config, claims are read only after the signature verifies, and a gateway token never reaches a provider.
- A gateway on loopback without `[auth]` serves anyone who can reach the port, other users of that machine too. It refuses what a web page sends (a `Host` other than a loopback name, an `Origin`, or a cross-site `Sec-Fetch-Site`), so a page you visit cannot use it through DNS rebinding.

Whoever takes over the gateway's host gets the provider keys and requests in flight. There is no key database, spend history, or stored prompt to take.

## Releases

- Release archives are built by GitHub Actions from a version tag, with checksums and a build-provenance attestation. `gh attestation verify <archive> --repo askrid/wye` checks the repository, workflow, and commit that built it.
- The shell installer, `wye-installer.sh`, is built by the same release run and published with it. It checks the archive it downloads against the SHA-256 written into it; the installer itself has no attestation.
- A release publishes only a commit whose `ci` run on `main` passed; the release workflow checks before it creates the release.
- Release tags (`v*`) are protected by a ruleset. Only a repository admin can create, move, or delete one.
- Every workflow pins its actions to a commit, and the `Dockerfile` pins its base images by digest.
- The container image, `ghcr.io/askrid/wye`, is built in the release run from the Linux archives after `gh attestation verify` passes on them, so it runs the attested binary. `gh attestation verify oci://ghcr.io/askrid/wye:<version> --repo askrid/wye` checks the image's own build-provenance attestation, stored in the registry beside it.
- No publishing credential is stored anywhere. Releases and the image use the run's own token, and nothing is published to crates.io or PyPI.
- Dependencies come only from crates.io, locked by checksum in `Cargo.lock` and checked by cargo-deny on every change and weekly for new advisories. The binary runs no code but its own at startup.

Still missing are immutable releases (an admin token can still replace a release's files, which attestation verification would then reject), a checksum on the `dist` installer the release jobs download, reproducible builds, two-person review (one maintainer), and secrets cleared from memory.

## Compared with LiteLLM's 2026 compromises

LiteLLM was backdoored twice in 2026 ([March](https://snyk.io/blog/poisoned-security-scanner-backdooring-litellm/), [October](https://docs.litellm.ai/blog/security-incident-oct-21-2026-attack-chain)). A compromised CI action exposed its PyPI publishing credentials, and a leaked admin key led to a release built from a look-alike repository and run by a `.pth` file at Python startup. The release measures above answer both. None of them stops a stolen maintainer token from tagging a release. Tags are protected, but the maintainer's own token can still create one.
