#!/usr/bin/env bash
# Latency bench: an engine build against the fake satellite (questions from wavs.sh, answers saved, nothing
# played) and the fake Home Assistant (reads from the real one in .env, service calls only timed, never
# made). run.sh <name> <voice-engine binary> <rounds> [engine flags...]; needs `make kokoro` and the fake_satellite example
# built. A rendered speech cache in target/latency-bench/seed is copied in first, if there is one.
# shellcheck disable=SC1091 # .env is git-ignored, so shellcheck cannot follow it.
set -euo pipefail
cd "$(dirname "$0")/../.."
HERE=scripts/latency-bench B=target/latency-bench NAME=$1 BIN=$2 ROUNDS=$3
shift 3
OUT=$B/$NAME; rm -rf "$OUT"; mkdir -p "$OUT"
REAL=$(set -a; . ./.env; echo "$HA_URL")
python3 -I $HERE/fake_ha.py "$REAL" 18123 "$OUT/ha.tsv" & HA=$!
# QUESTIONS_DIR picks other questions than wavs.sh made.
QUESTIONS=$(for _ in $(seq "$ROUNDS"); do printf '%s,' "${QUESTIONS_DIR:-$B/wav}"/question*.wav; done); QUESTIONS=${QUESTIONS%,}
target/release/examples/fake_satellite 127.0.0.1:16053 "$QUESTIONS" "$OUT/answers.wav" > "$OUT/satellite.txt" 2>&1 & SAT=$!
sleep 1
if [ -d $B/seed ]; then cp -R $B/seed "$OUT/cache"; fi
# A clean environment: the engine sees no Home Assistant, Hue or other setting from .env or the shell but
# the fake's, since an environment variable wins over its settings file.
TOKEN=$(set -a; . ./.env; echo "$HA_TOKEN")
env -i PATH="$PATH" HOME="$HOME" RUST_LOG="${RUST_LOG:-info}" HA_TOKEN="$TOKEN" "$BIN" --config $HERE/bench.yaml \
  --ha-url http://127.0.0.1:18123 --speech-cache "$OUT/cache" "$@" > "$OUT/engine.log" 2>&1 & ENG=$!
wait $SAT || echo "the fake satellite exited with $?"
kill $ENG $HA 2>/dev/null || true
wait 2>/dev/null || true
grep -q 'url="http://127.0.0.1:18123"' "$OUT/engine.log" || echo "WARNING: the engine did not use the fake Home Assistant"
echo "done: $OUT"
