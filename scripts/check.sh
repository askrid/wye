#!/usr/bin/env bash
# Correctness and performance gate. Run before every commit (the pre-commit hook does).
#
#   scripts/check.sh   fmt, clippy, rustdoc, tests, then the benchmark against HEAD
#
# The benchmark runs this checkout and its parent commit in alternating rounds
# and fails on a metric past its limit in a run and its rerun
# (scripts/bench-gate.sh). Two variables adjust it, also through the hook:
#
#   WYE_GATE_AGAINST=<ref>        compare with another commit: HEAD~1 when
#                                 amending, a release to look for slow drift
#   WYE_GATE_ACCEPT=<pattern,...> let these metrics regress, only for an
#                                 approved tradeoff; give the numbers and the
#                                 reason in the commit message
set -euo pipefail
cd "$(dirname "$0")/.."

if (( $# )); then echo "usage: scripts/check.sh (no arguments; see the header)" >&2; exit 2; fi
against=${WYE_GATE_AGAINST:-HEAD}
accept=${WYE_GATE_ACCEPT:-}

# Runs a step and reports how long it took.
step() {
  local name=$1; shift
  local start=$SECONDS
  echo "==> $name"
  "$@"
  echo "    $name: $((SECONDS - start))s"
}

# The proxy tests and the benchmark open thousands of sockets (2,000 paced
# streams, each a client and an upstream connection); macOS allows 256 open
# files by default.
if (( $(ulimit -n) < 8192 )); then ulimit -n 8192 2>/dev/null || true; fi

step fmt cargo fmt --all --check
# --locked: a stale Cargo.lock fails here, as in CI, instead of being
# rewritten and left out of the commit.
step clippy cargo clippy --locked --all-targets --quiet -- -D warnings
# The only unsafe code (alloc_stats.rs) builds only with this feature.
step "clippy alloc-stats" cargo clippy --locked --quiet -p wye --features alloc-stats -- -D warnings
step rustdoc env RUSTDOCFLAGS='-D warnings' cargo doc --locked --quiet --no-deps --workspace --document-private-items
step test cargo test --locked --quiet
step "build release" cargo build --locked --release --quiet -p wye -p wye-bench
# Allocation-counting build, in its own target dir so it never replaces the measured binary.
step "build alloc-stats" cargo build --locked --release --quiet -p wye --features alloc-stats --target-dir target/alloc

step bench scripts/bench-gate.sh "$against" target/bench --allow-new ${accept:+--accept "$accept"}
