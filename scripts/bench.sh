#!/usr/bin/env bash
# Benchmarks two Wye builds in alternating rounds on this machine, so clock
# speed, heat, and background load weigh on both alike: a stored result
# from another day would carry that day's machine state.
#
#   scripts/bench.sh gate <ref> [compare flags...]  this checkout against a commit
#   scripts/bench.sh ci <ref> [<summary-file>]       the same, gating only what
#                                                   no machine changes
#   scripts/bench.sh ab <wye-a> <wye-b> [rounds]     two binaries: every metric
#                                                   and b's change, no verdict
#
# gate: builds <ref> in a worktree (target/reference, kept between runs, so a
# new reference rebuilds only what changed), then `wye-bench gate` runs both
# commits in alternating rounds, every side with its own wye-bench, compares
# them under both commits' limits, and reruns once on a regression; it fails
# only on a metric past its limit in both runs, or on any failed request or
# stream (crates/wye-bench/src/gate.rs). Results go to target/bench
# (target/ci-bench for ci); compare.txt holds the last comparison.
#
# ci: gate limited to heap allocations, failed requests, and failed streams,
# for shared CI runners, which are too noisy to gate latency or CPU; the
# whole comparison goes to <summary-file> for review.
#
# ab: for effects below the gate's tolerances. `wye-bench alternate` runs
# each binary once per round (median of 3 each), then `wye-bench diff`.
set -euo pipefail

usage() {
  sed -n '6,10p' "$0" | sed 's/^# //' >&2
  exit 2
}
command=${1:-}
case $command in
  gate | ci) (( $# >= 2 )) || usage ;;
  # Resolved before the cd, so paths relative to the caller's directory work.
  ab) (( $# >= 3 )) || usage; set -- ab "$(realpath "$2")" "$(realpath "$3")" "${4:-3}" ;;
  *) usage ;;
esac
cd "$(dirname "$0")/.."
# One heavy job at a time on this machine (scripts/machine-lock.sh).
[[ -n ${WYE_MACHINE_LOCKED:-} ]] || exec scripts/machine-lock.sh scripts/bench.sh "$@"
# Thousands of sockets: 2,000 paced streams, each a client and an upstream
# connection; macOS allows 256 open files by default.
if (( $(ulimit -n) < 8192 )); then ulimit -n 8192 2>/dev/null || true; fi
# A pre-commit hook gets GIT_INDEX_FILE pointing at the index being
# committed; the worktree checkout must not write to it.
unset GIT_INDEX_FILE GIT_DIR GIT_WORK_TREE
tree=target/reference

build() {
  (cd "$1" &&
    cargo build --locked --release --quiet -p wye -p wye-bench &&
    cargo build --locked --release --quiet -p wye --features alloc-stats --target-dir target/alloc)
}

# gate <ref> <out> [compare flags...]
gate() {
  local ref=$1 out=$2 sha start=$SECONDS; shift 2
  sha=$(git rev-parse --verify --quiet "$ref^{commit}") || { echo "unknown commit: $ref" >&2; exit 2; }
  if [[ -e $tree/.git ]]; then
    git -C "$tree" checkout --quiet --detach --force "$sha"
  else
    # A restored build cache (CI) holds the tree's target/ but no worktree:
    # keep the build across the worktree's re-creation.
    rm -rf target/reference-target
    if [[ -d $tree/target ]]; then mv "$tree/target" target/reference-target; fi
    rm -rf "$tree"
    git worktree prune
    git worktree add --quiet --detach "$tree" "$sha"
    if [[ -d target/reference-target ]]; then mv target/reference-target "$tree/target"; fi
  fi
  build "$tree"
  build .
  echo "    build $(git rev-parse --short "$sha"): $((SECONDS - start))s"
  echo "==> benchmark against $ref"
  ./target/release/wye-bench gate --reference "$tree" --current . --out "$out" "$@"
}

case $command in
  gate)
    gate "$2" target/bench "${@:3}"
    ;;
  ci)
    out=target/ci-bench gated='alloc_*,*_failed_requests,*_failed_streams' status=0
    summary=${3:-/dev/null}
    # In a subshell: a failing gate exits, and the summary is written anyway.
    ( gate "$2" "$out" --only "$gated" --allow-new ) || status=$?
    {
      echo "## Benchmark against \`$2\`"
      echo
      echo "Gated: \`$gated\`. Other rows past their limit read \`report\`: latency and CPU on a shared runner are informational."
      echo
      echo '```'
      cat "$out/compare.txt" 2>/dev/null || echo "(no comparison: the benchmark did not finish)"
      echo '```'
    } >> "$summary"
    exit "$status"
    ;;
  ab)
    cargo build --locked --release --quiet -p wye-bench
    bench=./target/release/wye-bench out=target/bench/ab
    "$bench" alternate --side "a=$bench,$2" --side "b=$bench,$3" \
      --rounds "$4" --repeat 3 --out "$out"
    "$bench" diff "$out/a.json" "$out/b.json"
    ;;
esac
