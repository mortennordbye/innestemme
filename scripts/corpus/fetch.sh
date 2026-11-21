#!/usr/bin/env bash
# Every text the live engine heard (its "heard" lines in Loki), deduplicated, to target/corpus/heard.txt
# as `lang<TAB>text` lines, for compare.sh. The texts are household speech: they stay in target/,
# which git ignores. Wake word checks are not requests and are left out.
#   fetch.sh [days back, default 90]
# Needs Loki port-forwarded to LOKI (default http://127.0.0.1:3100):
#   kubectl --context admin@genesis -n monitoring port-forward svc/loki 3100:3100
set -euo pipefail
cd "$(dirname "$0")/../.."
DAYS=${1:-90} LOKI=${LOKI:-http://127.0.0.1:3100}
OUT=target/corpus
mkdir -p "$OUT"
TO=$(date +%s) STEP=21600
FROM=$((TO - DAYS * 86400))
RAW=$OUT/heard.raw
: > "$RAW"
# Loki returns at most 5000 lines per query: six hours at a time.
for ((t = FROM; t < TO; t += STEP)); do
  curl -sfG "$LOKI/loki/api/v1/query_range" \
    --data-urlencode 'query={namespace="innestemme",container="innestemme"} |= "voice_engine::assistant: heard text="' \
    --data-urlencode "start=${t}000000000" --data-urlencode "end=$((t + STEP))000000000" \
    --data-urlencode limit=5000 --data-urlencode direction=forward |
    python3 -I scripts/corpus/heard.py >> "$RAW"
done
sort -u "$RAW" > "$OUT/heard.txt"
rm "$RAW"
echo "$(wc -l < "$OUT/heard.txt" | tr -d ' ') unique texts in $OUT/heard.txt"
