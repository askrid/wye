#!/usr/bin/env bash
# Benchmarks this checkout against another commit on the same machine and
# gates what does not depend on the machine: heap allocations and failed
# requests. Latency and CPU are printed for review but never fail the check,
# because shared CI runners are too noisy to gate them; the local pre-commit
# gate (scripts/check.sh) does that on a quiet machine.
#
#   scripts/ci-bench.sh <base-ref> [<summary-file>]
#
# Both sides run as scripts/bench-against.sh runs them: alternating rounds,
# each with its own wye-bench.
set -euo pipefail
cd "$(dirname "$0")/.."

base_ref=${1:?usage: scripts/ci-bench.sh <base-ref> [summary-file]}
summary=${2:-/dev/null}
out=target/ci-bench

echo "==> build and benchmark against $base_ref"
scripts/bench-against.sh "$base_ref" "$out"

gated='alloc_*,*_failed_requests,*_failed_streams'
{
  echo "## Benchmark against \`$base_ref\`"
  echo
  echo "Gated: \`$gated\`. Other rows past their limit read \`report\`: latency and CPU on a shared runner are informational."
  echo
  echo '```'
  ./target/release/wye-bench compare "$out/reference.json" "$out/current.json" --only "$gated" --allow-new || true
  echo '```'
} | tee -a "$summary"

./target/release/wye-bench compare "$out/reference.json" "$out/current.json" \
  --only "$gated" --allow-new >/dev/null
