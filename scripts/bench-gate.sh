#!/usr/bin/env bash
# Benchmarks this checkout against a commit (scripts/bench-against.sh) and
# compares the medians. On a regression it reruns the benchmark once and
# fails only when the same metric is past its limit again: load pushes some
# metric past its limit now and then, a different one each time, while a real
# regression repeats.
#
#   scripts/bench-gate.sh <ref> <out-dir> [wye-bench compare flags...]
#
# Prints each comparison; <out-dir>/compare.txt holds the last one.
set -euo pipefail
cd "$(dirname "$0")/.."

usage="usage: scripts/bench-gate.sh <ref> <out-dir> [compare flags...]"
ref=${1:?$usage}
out=${2:?$usage}
shift 2

# Prints the comparison and writes the failing metrics' names to $1; fails
# when there are any.
compare() {
  local failed=$1; shift
  ./target/release/wye-bench compare "$out/reference.json" "$out/current.json" "$@" \
    | tee "$out/compare.txt"
  local status=${PIPESTATUS[0]}
  awk '$NF == "REGRESSED" || $NF == "missing" { print $1 }' "$out/compare.txt" | sort > "$failed"
  return "$status"
}

scripts/bench-against.sh "$ref" "$out"
echo "==> compare with $ref"
if compare "$out/failed-1.txt" "$@"; then
  exit 0
fi
echo "==> regression reported; re-running the benchmark to rule out noise"
scripts/bench-against.sh "$ref" "$out"
if compare "$out/failed-2.txt" "$@"; then
  exit 0
fi
both=$(comm -12 "$out/failed-1.txt" "$out/failed-2.txt" | tr '\n' ' ')
if [[ -n $both ]]; then
  echo "past the limit in both runs: $both" >&2
  exit 1
fi
echo "==> no metric failed both runs: noise, not a regression"
