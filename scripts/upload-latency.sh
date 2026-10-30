#!/usr/bin/env bash
# Times a large request upload to the Anthropic API over HTTP/1.1 and HTTP/2,
# to see whether HTTP/2 flow control (api.anthropic.com advertises a 64 KiB
# initial stream window) slows agent-sized uploads. Uses count_tokens, which
# is free; it needs ANTHROPIC_API_KEY and makes 2 x ROUNDS requests.
#
#   ANTHROPIC_API_KEY=... scripts/upload-latency.sh [rounds]
#
# Set WYE_URL (for example http://127.0.0.1:8080) to also time the same
# request through a running Wye (with upstream.http_version "auto" or "1.1").
set -euo pipefail
rounds=${1:-5}
model=${MODEL:-claude-haiku-5-5}
: "${ANTHROPIC_API_KEY:?set ANTHROPIC_API_KEY}"
body=$(mktemp)
trap 'command rm -f "$body"' EXIT
# About 500 KB of text (~120k tokens), under the context window.
python3 -I - "$model" > "$body" <<'PY'
import json, sys
words = "the quick brown fox jumps over the lazy dog while the gateway relays tokens".split()
text = " ".join(words[i % len(words)] + str(i % 97) for i in range(60_000))
print(json.dumps({"model": sys.argv[1], "messages": [{"role": "user", "content": text}]}))
PY
echo "body: $(wc -c < "$body") bytes, model $model, $rounds rounds each"

time_one() { # url, curl protocol flag
    # The key goes in through stdin, not the command line, where ps shows it.
    curl -s -o /dev/null -w '%{http_code} %{time_starttransfer} %{time_total}\n' "$2" \
        --config - <<<"header = \"x-api-key: $ANTHROPIC_API_KEY\"" \
        -H "anthropic-version: 2023-06-01" \
        -H "content-type: application/json" --data-binary @"$body" "$1/v1/messages/count_tokens"
}
targets=("https://api.anthropic.com --http1.1" "https://api.anthropic.com --http2")
[ -n "${WYE_URL:-}" ] && targets+=("$WYE_URL --http1.1")
for target in "${targets[@]}"; do
    read -r url flag <<<"$target"
    printf '%-40s' "$url $flag"
    results=$(for _ in $(seq 1 "$rounds"); do time_one "$url" "$flag"; done)
    bad=$(awk '$1 != 200' <<<"$results" | wc -l | tr -d ' ')
    median=$(awk '{print $3}' <<<"$results" | sort -n | awk '{t[NR] = $1} END {print t[int((NR + 1) / 2)]}')
    echo "median total ${median}s ($bad non-200)"
done
