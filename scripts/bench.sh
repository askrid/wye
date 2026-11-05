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
# new reference rebuilds only what changed) and runs both commits three times
# each, every side with its own wye-bench, so a commit that changes the
# benchmark still measures the reference as that commit did. The medians are
# compared under both commits' limits, so a commit cannot pass by loosening
# the gate it is judged by. On a regression the benchmark runs again, and the
# gate fails only when the same metric is past its limit again: load pushes
# some metric past its limit now and then, a different one each time, while a
# real regression repeats. A failed request or stream is never noise: it
# fails at once. Results go to target/bench (target/ci-bench for ci);
# compare.txt holds the last comparison.
#
# ci: gate limited to heap allocations, failed requests, and failed streams,
# for shared CI runners, which are too noisy to gate latency or CPU; the
# whole comparison goes to <summary-file> for review.
#
# ab: for effects below the gate's tolerances. Each round runs wye-bench
# (median of 3) once per binary.
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

# Runs one side once: <label> <side>, where <side>_bench, <side>_wye, and
# <side>_alloc (empty for none) name its binaries; writes <out>/<side>-<i>.json.
run_side() {
  local label=$1 side=$2 file=$3 repeat=$4 start=$SECONDS
  local bench=${side}_bench wye=${side}_wye alloc=${side}_alloc
  # The metric table goes to the result file; the phase durations on stderr
  # stay visible.
  "${!bench}" run --wye "${!wye}" --out "$file" --repeat "$repeat" \
    ${!alloc:+--wye-alloc "${!alloc}"} >/dev/null
  echo "    $label: $((SECONDS - start))s"
}

# alternate <out> <rounds> <repeat> <first> <second>: runs the two sides in
# turn, swapping which goes first each round so a trend within the run
# splits evenly, then writes <out>/<side>.json, the median of its rounds.
alternate() {
  local out=$1 rounds=$2 repeat=$3 first=$4 second=$5 i side
  mkdir -p "$out"
  rm -f "$out/$first"-*.json "$out/$second"-*.json
  for i in $(seq 1 "$rounds"); do
    local order="$first $second"
    (( i % 2 )) || order="$second $first"
    for side in $order; do
      run_side "$side $i/$rounds" "$side" "$out/$side-$i.json" "$repeat"
    done
  done
  for side in "$first" "$second"; do
    ./target/release/wye-bench median "$out/$side.json" "$out/$side"-*.json
  done
}

build() {
  (cd "$1" &&
    cargo build --locked --release --quiet -p wye -p wye-bench &&
    cargo build --locked --release --quiet -p wye --features alloc-stats --target-dir target/alloc)
}

# against <ref> <out>: <out>/reference.json and <out>/current.json.
against() {
  local sha start=$SECONDS
  sha=$(git rev-parse --verify --quiet "$1^{commit}") || { echo "unknown commit: $1" >&2; exit 2; }
  if [[ -e $tree/.git ]]; then
    git -C "$tree" checkout --quiet --detach --force "$sha"
  else
    rm -rf "$tree"
    git worktree prune
    git worktree add --quiet --detach "$tree" "$sha"
  fi
  build "$tree"
  build .
  echo "    build $(git rev-parse --short "$sha"): $((SECONDS - start))s"
  reference_bench=$tree/target/release/wye-bench reference_wye=$tree/target/release/wye
  reference_alloc=$tree/target/alloc/release/wye
  current_bench=./target/release/wye-bench current_wye=./target/release/wye
  current_alloc=./target/alloc/release/wye
  alternate "$2" 3 1 reference current
}

# compare <out> <failed-file> [flags]: prints the comparison and writes the
# failing metrics' names to <failed-file>; fails when there are any. A
# comparison that fails without naming a metric (a panic, a bad report) ends
# the gate: there is no run to rerun against.
compare() {
  local out=$1 failed=$2; shift 2
  ./target/release/wye-bench compare "$out/reference.json" "$out/current.json" "$@" \
    | tee "$out/compare.txt"
  local status=${PIPESTATUS[0]}
  # The reference's own limits too; only what they add is printed. A
  # reference too old for these flags fails without naming a metric, and is
  # skipped.
  local ref_status=0 stricter
  "$tree/target/release/wye-bench" compare "$out/reference.json" "$out/current.json" \
    "$@" > "$out/compare-reference-limits.txt" 2>&1 || ref_status=$?
  stricter=$(awk '$NF == "REGRESSED" || $NF == "missing" { print $1 }' \
    "$out/compare-reference-limits.txt")
  if [[ $ref_status -ne 0 && -n $stricter ]]; then
    echo "past the reference's own limits: $(tr '\n' ' ' <<<"$stricter")"
    status=1
  elif [[ $ref_status -ne 0 ]]; then
    echo "note: the reference's wye-bench could not compare; its limits are skipped" >&2
  fi
  { awk '$NF == "REGRESSED" || $NF == "missing" { print $1 }' "$out/compare.txt"
    echo "$stricter"; } | grep -v '^$' | sort -u > "$failed" || true
  if [[ $status -ne 0 && ! -s $failed ]]; then
    echo "the comparison failed (exit $status) without naming a metric" >&2
    exit 1
  fi
  return "$status"
}

# A failed request or stream is not noise, in either run.
stop_on_failures() {
  local failures
  if failures=$(grep -E '_failed_(requests|streams)$' "$1"); then
    echo "requests or streams failed: $(tr '\n' ' ' <<<"$failures")" >&2
    exit 1
  fi
}

# gate <ref> <out> [compare flags...]
gate() {
  local ref=$1 out=$2; shift 2
  against "$ref" "$out"
  echo "==> compare with $ref"
  compare "$out" "$out/failed-1.txt" "$@" && return 0
  stop_on_failures "$out/failed-1.txt"
  echo "==> regression reported; re-running the benchmark to rule out noise"
  against "$ref" "$out"
  compare "$out" "$out/failed-2.txt" "$@" && return 0
  stop_on_failures "$out/failed-2.txt"
  local both
  both=$(comm -12 "$out/failed-1.txt" "$out/failed-2.txt" | tr '\n' ' ')
  if [[ -n $both ]]; then
    echo "past the limit in both runs: $both" >&2
    exit 1
  fi
  echo "==> no metric failed both runs: noise, not a regression"
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
    a_bench=./target/release/wye-bench a_wye=$2 a_alloc=
    b_bench=./target/release/wye-bench b_wye=$3 b_alloc=
    alternate target/bench/ab "$4" 3 a b
    ./target/release/wye-bench diff target/bench/ab/a.json target/bench/ab/b.json
    ;;
esac
