#!/usr/bin/env bash
# The website's check: formatting, lints, tests, a build of site/out (which
# fails on a missing source, a broken local link, or a resource from another
# origin), and the page budget. The pre-commit hook runs it for a commit that
# changes the site or a file the site reads. It never builds the gateway.
set -euo pipefail
cd "$(dirname "$0")"

if (( $# )); then echo "usage: site/check.sh (no arguments; see the header)" >&2; exit 2; fi

# The build shares this machine with the gateway's commit gate: run under the
# machine lock, so it never lands in the middle of a benchmark.
if [[ -z ${SITE_CHECK_LOCKED:-} ]]; then
  SITE_CHECK_LOCKED=1 exec ../scripts/machine-lock.sh ./check.sh
fi

cargo fmt --check
cargo clippy --locked --all-targets --quiet -- -D warnings
cargo test --locked --quiet
cargo run --locked --quiet

# What a first visit to one page downloads, compressed as a server sends it:
# the page and every stylesheet, script, and SVG gzipped, the fonts as they
# are. The footer of pages/index.html states this budget.
budget=$((150 * 1024))
gz() { gzip -9c "$1" | wc -c; }
shared=$(cat out/assets/fonts/*.woff2 | wc -c)
for asset in out/assets/*.css out/assets/*.js out/assets/*.svg; do
  shared=$(( shared + $(gz "$asset") ))
done
for page in out/*.html; do
  total=$(( $(gz "$page") + shared ))
  echo "site: ${page#out/} weighs $(( total / 1024 )) KiB of its $(( budget / 1024 )) KiB budget"
  if (( total > budget )); then
    echo "site: ${page#out/} is over budget" >&2
    exit 1
  fi
done
