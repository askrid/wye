#!/usr/bin/env bash
# Benchmarks this checkout and another commit in alternating rounds on this
# machine, so clock speed, heat, and background load weigh on both alike: a
# stored result from another day would carry that day's machine state.
#
#   scripts/bench-against.sh <ref> <out-dir> [rounds]
#
# Writes <out-dir>/reference.json (the commit) and <out-dir>/current.json
# (this checkout), each the median of `rounds` runs (default 3). Each side runs
# its own wye-bench, so a commit that changes the benchmark still measures
# the reference as that commit did. The reference is built in a worktree,
# target/reference, kept between runs: a new reference rebuilds only what
# changed.
set -euo pipefail
cd "$(dirname "$0")/.."

usage="usage: scripts/bench-against.sh <ref> <out-dir> [rounds]"
ref=${1:?$usage}
out=${2:?$usage}
rounds=${3:-3}
tree=target/reference

# A pre-commit hook gets GIT_INDEX_FILE pointing at the index being committed;
# the worktree checkout must not write to it.
unset GIT_INDEX_FILE GIT_DIR GIT_WORK_TREE
if (( $(ulimit -n) < 8192 )); then ulimit -n 8192 2>/dev/null || true; fi

sha=$(git rev-parse --verify --quiet "$ref^{commit}") || { echo "unknown commit: $ref" >&2; exit 2; }
if [[ -e $tree/.git ]]; then
  git -C "$tree" checkout --quiet --detach --force "$sha"
else
  rm -rf "$tree"
  git worktree prune
  git worktree add --quiet --detach "$tree" "$sha"
fi

build() {
  (cd "$1" &&
    cargo build --locked --release --quiet -p wye -p wye-bench &&
    cargo build --locked --release --quiet -p wye --features alloc-stats --target-dir target/alloc)
}

# The metric table goes to the result file, not the terminal; the phase
# durations on stderr stay visible.
run() {
  local start=$SECONDS
  "$1/target/release/wye-bench" run --wye "$1/target/release/wye" --out "$3" \
    --wye-alloc "$1/target/alloc/release/wye" --repeat 1 >/dev/null
  echo "    $2: $((SECONDS - start))s"
}

start=$SECONDS
build "$tree"
build .
echo "    build $(git rev-parse --short "$sha"): $((SECONDS - start))s"

mkdir -p "$out"
rm -f "$out"/reference-*.json "$out"/current-*.json
# Alternate which side goes first, so a trend within the run splits evenly.
for i in $(seq 1 "$rounds"); do
  if (( i % 2 )); then
    run "$tree" "reference $i/$rounds" "$out/reference-$i.json"
    run . "current $i/$rounds" "$out/current-$i.json"
  else
    run . "current $i/$rounds" "$out/current-$i.json"
    run "$tree" "reference $i/$rounds" "$out/reference-$i.json"
  fi
done
./target/release/wye-bench median "$out/reference.json" "$out"/reference-*.json
./target/release/wye-bench median "$out/current.json" "$out"/current-*.json
