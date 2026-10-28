#!/usr/bin/env bash
# Benchmarks this checkout against another commit on the same machine and
# gates what does not depend on the machine: heap allocations and failed
# requests. Latency and CPU are printed for review but never fail the check,
# because shared CI runners are too noisy to gate them; the local pre-commit
# gate (scripts/check.sh) does that on a quiet machine.
#
#   scripts/ci-bench.sh <base-ref> [<summary-file>]
#
# The base commit is built in a worktree under target/, with its own
# wye-bench, so a pull request that changes the benchmark still compares
# like with like.
set -euo pipefail
cd "$(dirname "$0")/.."

base_ref=${1:?usage: scripts/ci-bench.sh <base-ref> [summary-file]}
summary=${2:-/dev/null}
out=target/ci-bench
base_tree=$out/base

mkdir -p "$out"
ulimit -n 65536 2>/dev/null || ulimit -n 8192

build() {
  (cd "$1" &&
    cargo build --locked --release --quiet -p wye -p wye-bench &&
    cargo build --locked --release --quiet -p wye --features alloc-stats --target-dir target/alloc)
}

run() {
  local tree=$1 result=$2
  "$tree/target/release/wye-bench" run --wye "$tree/target/release/wye" --out "$result" \
    --wye-alloc "$tree/target/alloc/release/wye" >/dev/null
}

echo "==> build this checkout"
build .
echo "==> build $base_ref"
git worktree remove --force "$base_tree" 2>/dev/null || true
git worktree add --detach "$base_tree" "$base_ref"
trap 'git worktree remove --force "$base_tree" 2>/dev/null || true' EXIT
build "$base_tree"

echo "==> benchmark $base_ref"
run "$base_tree" "$out/base.json"
echo "==> benchmark this checkout"
run . "$out/head.json"

gated='alloc_*,*_failed_requests,*_failed_streams'
{
  echo "## Benchmark against \`$base_ref\`"
  echo
  echo "Gated: \`$gated\`. Latency and CPU rows marked \`report\` are informational (shared runner)."
  echo
  echo '```'
  ./target/release/wye-bench compare "$out/base.json" "$out/head.json" --only "$gated" --allow-new || true
  echo '```'
} | tee -a "$summary"

./target/release/wye-bench compare "$out/base.json" "$out/head.json" \
  --only "$gated" --allow-new >/dev/null
