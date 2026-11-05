#!/usr/bin/env bash
# Correctness and performance gate. Run before every commit (the pre-commit hook does).
#
#   scripts/check.sh   fmt, clippy, rustdoc, tests, then the benchmark against HEAD
#
# The benchmark runs this checkout and its parent commit in alternating rounds
# and fails on a metric past its limit in a run and its rerun
# (scripts/bench.sh gate). Three variables adjust it, also through the hook:
#
#   WYE_GATE_AGAINST=<ref>        compare with another commit: HEAD~1 when
#                                 amending, a release to look for slow drift,
#                                 a series' base for its last commit
#   WYE_GATE_ACCEPT=<pattern,...> let these metrics regress, only for an
#                                 approved tradeoff; give the numbers and the
#                                 reason in the commit message
#   WYE_GATE_BENCH=series         skip the release builds and the benchmark,
#                                 for a commit inside a series whose tip is
#                                 benchmarked against the series' base before
#                                 any of it is pushed (docs/development.md)
set -euo pipefail
cd "$(dirname "$0")/.."

if (( $# )); then echo "usage: scripts/check.sh (no arguments; see the header)" >&2; exit 2; fi
# One heavy job at a time on this machine: builds and benchmarks hold the lock.
[[ -n ${WYE_MACHINE_LOCKED:-} ]] || exec scripts/machine-lock.sh scripts/check.sh
against=${WYE_GATE_AGAINST:-HEAD}
accept=${WYE_GATE_ACCEPT:-}
bench=${WYE_GATE_BENCH:-each}
if [[ $bench != each && $bench != series ]]; then
  echo "WYE_GATE_BENCH is each (the default) or series, not '$bench'" >&2; exit 2
fi

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
if [[ $bench == series ]]; then
  echo "==> bench: skipped (WYE_GATE_BENCH=series); before pushing, run"
  echo "    WYE_GATE_AGAINST=<the series' base> scripts/check.sh on its tip"
  exit 0
fi
step "build release" cargo build --locked --release --quiet -p wye -p wye-bench
# Allocation-counting build, in its own target dir so it never replaces the measured binary.
step "build alloc-stats" cargo build --locked --release --quiet -p wye --features alloc-stats --target-dir target/alloc

step bench scripts/bench.sh gate "$against" --allow-new ${accept:+--accept "$accept"}
