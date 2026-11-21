#!/usr/bin/env bash
# Replays a day from fetch.sh through an engine build: the fake satellite streams the day's runs as
# one microphone (gaps capped at 30 s), the fake Home Assistant reads the real one and never makes a
# service call. Nothing plays anywhere. Then report.py compares it with the live log.
#   run.sh <name> <voice-engine binary> <day> [engine flags...]
# Needs the fake_satellite example built (cargo build --release -p voice-esphome --example fake_satellite).
# The speech cache is kept and reused in target/replay/cache/<name>; only sentences not in it are
# rendered. The name is the voice and the sherpa-onnx version in Cargo.lock, which change how speech
# sounds; a build that changes it in another way gets its own with CACHE=<name>.
set -euo pipefail
cd "$(dirname "$0")/../.."
HERE=scripts/replay B=target/replay NAME=$1 BIN=$2 DAY=$3
shift 3
OUT=$B/$NAME; rm -rf "$OUT"; mkdir -p "$OUT"
SHERPA=$(awk '/^name = "sherpa-onnx"$/ { getline; gsub(/[^0-9.]/, ""); print }' Cargo.lock)
VOICE=$(awk '/^kokoro-voice:/ { print $2 }' $HERE/replay.yaml)
CACHE=$B/cache/${CACHE:-$VOICE-sherpa-$SHERPA}; mkdir -p "$CACHE"
python3 -I $HERE/stream.py "$B/days/$DAY" "$OUT"
REAL=$(set -a; . ./.env; echo "$HA_URL")
python3 -I scripts/latency-bench/fake_ha.py "$REAL" 18223 "$OUT/ha.tsv" & HA=$!
trap 'kill $HA ${ENG:-} 2>/dev/null || true' EXIT
# A clean environment: the engine sees no Home Assistant, Hue or other setting but the fake's.
TOKEN=$(set -a; . ./.env; echo "$HA_TOKEN")
env -i PATH="$PATH" HOME="$HOME" RUST_LOG=info HA_TOKEN="$TOKEN" "$BIN" --config $HERE/replay.yaml \
  --ha-url http://127.0.0.1:18223 --speech-cache "$CACHE" "$@" > "$OUT/engine.log" 2>&1 & ENG=$!
until grep -q 'listener warmed up' "$OUT/engine.log"; do kill -0 $ENG || { tail "$OUT/engine.log"; exit 1; }; sleep 2; done
# The engine renders its fixed sentences ahead first and pauses that while a request runs. A new
# cache takes hours for the long tail, so wait only while the common ones come (CACHE_WAIT, s).
last=-1 waited=0
while n=$(find "$CACHE" -type f | wc -l); [ $((n - last)) -gt 5 ] && [ $waited -lt "${CACHE_WAIT:-600}" ]; do
  last=$n; sleep 20; waited=$((waited + 20))
done
grep -Eq 'home assistant connected url="?http://127.0.0.1:18223' "$OUT/engine.log" ||
  { echo "the engine is not on the fake Home Assistant; stopped"; exit 1; }
echo "engine ready, $n sentences in the cache; replaying $(awk 'END { print NR - 1 }' "$OUT/runs.tsv") runs"
target/release/examples/fake_satellite 127.0.0.1:16153 "$OUT/stream.wav" /dev/null --continuous > "$OUT/satellite.txt" 2>&1 ||
  echo "the fake satellite exited with $?"
python3 -I $HERE/report.py "$B/days/$DAY" "$OUT"
