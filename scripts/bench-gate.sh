#!/usr/bin/env bash
# Benchmarks this checkout against a commit (scripts/bench-against.sh) and
# compares the medians. On a regression it reruns the benchmark once and
# fails only when the same metric is past its limit again: load pushes some
# metric past its limit now and then, a different one each time, while a real
# regression repeats. A failed request or stream is never noise: it fails the
# gate at once, without a rerun to excuse it.
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
# when there are any. A comparison that fails without naming a metric (a
# panic, a bad report) ends the gate: there is no run to rerun against.
compare() {
  local failed=$1; shift
  ./target/release/wye-bench compare "$out/reference.json" "$out/current.json" "$@" \
    | tee "$out/compare.txt"
  local status=${PIPESTATUS[0]}
  # The reference's own limits too (its wye-bench, built by bench-against.sh),
  # so a commit cannot pass by loosening the gate it is judged by. Only what
  # they add is printed. A reference too old for these flags fails without
  # naming a metric, and is skipped.
  local ref_status=0
  target/reference/target/release/wye-bench compare "$out/reference.json" "$out/current.json" \
    "$@" > "$out/compare-reference-limits.txt" 2>&1 || ref_status=$?
  local stricter
  stricter=$(awk '$NF == "REGRESSED" || $NF == "missing" { print $1 }' \
    "$out/compare-reference-limits.txt")
  if [[ $ref_status -ne 0 && -n $stricter ]]; then
    echo "past the reference's own limits: $(tr '\n' ' ' <<<"$stricter")"
    status=1
  elif [[ $ref_status -ne 0 ]]; then
    echo "note: the reference's wye-bench could not compare; its limits are skipped" >&2
  fi
  # An empty $stricter adds one empty line, which sort -u keeps once; drop it.
  { awk '$NF == "REGRESSED" || $NF == "missing" { print $1 }' "$out/compare.txt"
    echo "$stricter"; } | grep -v '^$' | sort -u > "$failed" || true
  if [[ $status -ne 0 && ! -s $failed ]]; then
    echo "the comparison failed (exit $status) without naming a metric" >&2
    exit 1
  fi
  return "$status"
}

scripts/bench-against.sh "$ref" "$out"
echo "==> compare with $ref"
if compare "$out/failed-1.txt" "$@"; then
  exit 0
fi
# A failed request or stream is not noise, in either run.
stop_on_failures() {
  local failures
  if failures=$(grep -E '_failed_(requests|streams)$' "$1"); then
    echo "requests or streams failed: $(tr '\n' ' ' <<<"$failures")" >&2
    exit 1
  fi
}
stop_on_failures "$out/failed-1.txt"
echo "==> regression reported; re-running the benchmark to rule out noise"
scripts/bench-against.sh "$ref" "$out"
if compare "$out/failed-2.txt" "$@"; then
  exit 0
fi
stop_on_failures "$out/failed-2.txt"
both=$(comm -12 "$out/failed-1.txt" "$out/failed-2.txt" | tr '\n' ' ')
if [[ -n $both ]]; then
  echo "past the limit in both runs: $both" >&2
  exit 1
fi
echo "==> no metric failed both runs: noise, not a regression"
