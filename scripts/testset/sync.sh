#!/usr/bin/env bash
# Copies the live engine's recordings that are not here yet to target/testset/: runs/run-*.wav
# (16 kHz, wake word to the end of the answer) and utterances/utterance-*.wav (24 kHz, each request
# as the engine cut it). The cluster keeps them for a year on a backed-up volume, so this is a cache.
# Needs the cluster context (KUBE_CONTEXT, default admin@genesis).
set -euo pipefail
cd "$(dirname "$0")/../.."
OUT=target/testset; mkdir -p "$OUT/runs" "$OUT/utterances"
# shellcheck source=scripts/recordings.sh
. scripts/recordings.sh

NEW=$(comm -13 <(find "$OUT/runs" "$OUT/utterances" -name '*.wav' -exec basename {} \; | sort) \
  <(k exec "$POD" -c "$NAME" -- ls "$REC" | grep -E '^(run|utterance)-[0-9]+\.wav$' | sort))
if [ -z "$NEW" ]; then echo "nothing new"; exit 0; fi
# shellcheck disable=SC2086
k exec "$POD" -c "$NAME" -- tar -C "$REC" -cf - $NEW | tar -C "$OUT" -xf -
mv "$OUT"/run-*.wav "$OUT/runs/" 2>/dev/null || true
mv "$OUT"/utterance-*.wav "$OUT/utterances/" 2>/dev/null || true
count() { find "$1" -name '*.wav' | wc -l | tr -d ' '; }
echo "$(echo "$NEW" | wc -l | tr -d ' ') new; $(count "$OUT/runs") runs, $(count "$OUT/utterances") utterances"
