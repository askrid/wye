#!/usr/bin/env bash
# A/B benchmark two wye binaries, alternating runs (and which goes first) so
# drift affects both equally.
#
#   scripts/ab.sh <wye-a> <wye-b> [rounds]
#
# Each round runs wye-bench (median of 3) once per binary. Prints the median of
# rounds per metric and B's change relative to A. Use it for any change whose
# expected effect is below the commit gate's sensitivity.
set -euo pipefail
cd "$(dirname "$0")/.."
a=$1; b=$2; rounds=${3:-3}
out=target/bench/ab; rm -rf "$out"; mkdir -p "$out"
if (( $(ulimit -n) < 8192 )); then ulimit -n 8192 2>/dev/null || true; fi
cargo build --release --quiet -p wye-bench
for i in $(seq 1 "$rounds"); do
  order="a b"; (( i % 2 == 0 )) && order="b a"
  for side in $order; do
    bin=$a; [[ $side == b ]] && bin=$b
    ./target/release/wye-bench run --wye "$bin" --out "$out/$side-$i.json" \
      >/dev/null 2>"$out/$side-$i.log"
  done
  echo "round $i/$rounds done" >&2
done
python3 -I - "$out" "$rounds" <<'PY'
import json, statistics, sys
out, rounds = sys.argv[1], int(sys.argv[2])
load = lambda s, i: json.load(open(f"{out}/{s}-{i}.json"))["metrics"]
keys = load("a", 1).keys()
print(f"{'metric':32} {'A':>10} {'B':>10} {'change':>8}")
# A metric without samples is written as null; it has no median.
median = lambda s, k: statistics.median(
    [v for i in range(1, rounds + 1) if (v := load(s, i).get(k)) is not None] or [float("nan")])
for k in keys:
    a, b = median("a", k), median("b", k)
    change = f"{100 * (b - a) / abs(a):+.1f}%" if a else "n/a"
    print(f"{k:32} {a:10.2f} {b:10.2f} {change:>8}")
PY
