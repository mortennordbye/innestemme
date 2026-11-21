#!/usr/bin/env bash
# Copies one day of the live engine's runs (dump-utterances: /models/recordings) and its log lines from Loki
# to target/replay/days/<day>/, for run.sh. fetch.sh <YYYY-MM-DD> (a day in Oslo time).
# The image has no shell or tar: a debug container with the engine's user reads its files through
# /proc/1/root. Needs the cluster context (KUBE_CONTEXT, default admin@genesis) and Loki port-forwarded
# to LOKI (default http://127.0.0.1:3100):
#   kubectl --context admin@genesis -n monitoring port-forward svc/loki 3100:3100
set -euo pipefail
cd "$(dirname "$0")/../.."
DAY=$1 CTX=${KUBE_CONTEXT:-admin@genesis} LOKI=${LOKI:-http://127.0.0.1:3100}
OUT=target/replay/days/$DAY; mkdir -p "$OUT/runs"
FROM=$(TZ=Europe/Oslo date -j -f "%Y-%m-%d %H:%M:%S" "$DAY 00:00:00" +%s)
TO=$((FROM + 86400))
k() { kubectl --context "$CTX" -n innestemme "$@"; }

POD=$(k get pod -l app=innestemme -o jsonpath='{.items[0].metadata.name}')
NAME=replay-$(date +%s)
k debug "$POD" --target=innestemme --container="$NAME" --image=busybox:1.37 --profile=restricted \
  --custom=<(printf '{"securityContext":{"runAsUser":65532,"runAsGroup":65532}}') -- sleep 600 >/dev/null
for _ in $(seq 30); do
  [ "$(k get pod "$POD" -o jsonpath="{.status.ephemeralContainerStatuses[?(@.name=='$NAME')].state.running}")" ] && break
  sleep 1
done
# File names hold the save time in ms; only that day's runs.
FILES=$(k exec "$POD" -c "$NAME" -- ls /proc/1/root/models/recordings |
  awk -F'[-.]' -v from="${FROM}000" -v to="${TO}000" '$1 == "run" && $2 >= from && $2 < to')
if [ -z "$FILES" ]; then echo "no runs on $DAY"; exit 1; fi
# shellcheck disable=SC2086
k exec "$POD" -c "$NAME" -- tar -C /proc/1/root/models/recordings -cf - $FILES | tar -C "$OUT/runs" -xf -
echo "$(echo "$FILES" | wc -l | tr -d ' ') runs"

# Loki returns at most 5000 lines per query: an hour at a time, oldest first.
: > "$OUT/live.log"
for ((t = FROM; t < TO; t += 3600)); do
  curl -sfG "$LOKI/loki/api/v1/query_range" --data-urlencode 'query={namespace="innestemme",container="innestemme"}' \
    --data-urlencode "start=${t}000000000" --data-urlencode "end=$((t + 3600))000000000" \
    --data-urlencode limit=5000 --data-urlencode direction=forward |
    python3 -I -c 'import json, sys
lines = [v for s in json.load(sys.stdin)["data"]["result"] for v in s["values"]]
for _, line in sorted(lines):
    sys.stdout.write(line if line.endswith("\n") else line + "\n")' >> "$OUT/live.log"
done
echo "$(wc -l < "$OUT/live.log" | tr -d ' ') log lines"
echo "done: $OUT"
