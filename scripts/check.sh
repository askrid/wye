#!/usr/bin/env bash
# Correctness and performance gate. Run before every commit (the pre-commit hook does).
#
#   scripts/check.sh   fmt, clippy, tests, then the benchmark against HEAD
#
# The benchmark runs this checkout and its parent commit in alternating rounds
# (scripts/bench-against.sh). A metric past its limit reruns the benchmark, and
# the check fails when the same metric is past its limit again. Two variables
# adjust it, also through the hook:
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

# Prints the comparison and writes the failing metrics' names to $1; fails
# when there are any.
compare() {
  ./target/release/wye-bench compare target/bench/reference.json target/bench/current.json \
    --allow-new ${accept:+--accept "$accept"} | tee target/bench/compare.txt
  local status=${PIPESTATUS[0]}
  awk '$NF == "REGRESSED" || $NF == "missing" { print $1 }' target/bench/compare.txt | sort > "$1"
  return "$status"
}

# The proxy tests and the benchmark open thousands of sockets (2,000 paced
# streams, each a client and an upstream connection); macOS allows 256 open
# files by default.
if (( $(ulimit -n) < 8192 )); then ulimit -n 8192 2>/dev/null || true; fi

step fmt cargo fmt --all --check
step clippy cargo clippy --all-targets --quiet -- -D warnings
step test cargo test --quiet
step "build release" cargo build --release --quiet -p wye -p wye-bench
# Allocation-counting build, in its own target dir so it never replaces the measured binary.
step "build alloc-stats" cargo build --release --quiet -p wye --features alloc-stats --target-dir target/alloc

step bench scripts/bench-against.sh "$against" target/bench
echo "==> compare with $against"
if compare target/bench/failed-1.txt; then
  exit 0
fi
# Load pushes some metric past its limit now and then, a different one each
# time; a real regression fails on the same metric again.
echo "==> regression reported; re-running the benchmark to rule out noise"
step bench scripts/bench-against.sh "$against" target/bench
if compare target/bench/failed-2.txt; then
  exit 0
fi
both=$(comm -12 target/bench/failed-1.txt target/bench/failed-2.txt | tr '\n' ' ')
if [[ -n $both ]]; then
  echo "past the limit in both runs: $both" >&2
  exit 1
fi
echo "==> no metric failed both runs: noise, not a regression"
