#!/usr/bin/env bash
# Correctness and performance gate. Run before every commit (the pre-commit hook does).
#
#   scripts/check.sh                    fmt, clippy, tests, benchmark vs bench/baseline.json
#   scripts/check.sh --update-baseline  same, then record the run as the new baseline
#
# A benchmark regression fails the check. Update the baseline only for an intended,
# reviewed tradeoff, and say why in the commit message.
set -euo pipefail
cd "$(dirname "$0")/.."

update=false
case "${1:-}" in
  "") ;;
  --update-baseline) update=true ;;
  *) echo "usage: scripts/check.sh [--update-baseline]" >&2; exit 2 ;;
esac

echo "==> fmt";    cargo fmt --all --check
echo "==> clippy"; cargo clippy --all-targets --quiet -- -D warnings
echo "==> test";   cargo test --quiet
echo "==> build release"; cargo build --release --quiet -p wye -p wye-bench
# Allocation-counting build, in its own target dir so it never replaces the measured binary.
cargo build --release --quiet -p wye --features alloc-stats --target-dir target/alloc

# 1000 idle streams need several thousand file descriptors.
if (( $(ulimit -n) < 8192 )); then ulimit -n 8192 2>/dev/null || true; fi

result=target/bench/latest.json
echo "==> bench"
./target/release/wye-bench run --wye target/release/wye --out "$result" --wye-alloc target/alloc/release/wye >/dev/null

if $update; then
  cp "$result" bench/baseline.json
  echo "baseline updated: bench/baseline.json"
  exit 0
fi
if [[ ! -f bench/baseline.json ]]; then
  echo "no bench/baseline.json; run scripts/check.sh --update-baseline" >&2
  exit 1
fi
echo "==> compare with bench/baseline.json"
if ./target/release/wye-bench compare bench/baseline.json "$result"; then
  exit 0
fi
# Laptop noise can push one run past a limit. A real regression fails twice in a row.
echo "==> regression reported; re-running the benchmark to rule out noise"
./target/release/wye-bench run --wye target/release/wye --out "$result" --wye-alloc target/alloc/release/wye >/dev/null
./target/release/wye-bench compare bench/baseline.json "$result"
