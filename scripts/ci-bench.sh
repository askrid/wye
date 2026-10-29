#!/usr/bin/env bash
# Benchmarks this checkout against another commit on the same machine and
# gates what does not depend on the machine: heap allocations and failed
# requests. Latency and CPU are printed for review but never fail the check,
# because shared CI runners are too noisy to gate them; the local pre-commit
# gate (scripts/check.sh) does that on a quiet machine.
#
#   scripts/ci-bench.sh <base-ref> [<summary-file>]
#
# Both sides run as scripts/bench-against.sh runs them (alternating rounds,
# each with its own wye-bench), and a regression counts as in the local gate:
# the same metric past its limit in a run and its rerun (scripts/bench-gate.sh).
set -euo pipefail
cd "$(dirname "$0")/.."

base_ref=${1:?usage: scripts/ci-bench.sh <base-ref> [summary-file]}
summary=${2:-/dev/null}
out=target/ci-bench
gated='alloc_*,*_failed_requests,*_failed_streams'

mkdir -p "$out"
status=0
scripts/bench-gate.sh "$base_ref" "$out" --only "$gated" --allow-new || status=$?
{
  echo "## Benchmark against \`$base_ref\`"
  echo
  echo "Gated: \`$gated\`. Other rows past their limit read \`report\`: latency and CPU on a shared runner are informational."
  echo
  echo '```'
  cat "$out/compare.txt" 2>/dev/null || echo "(no comparison: the benchmark did not finish)"
  echo '```'
} >> "$summary"
exit "$status"
