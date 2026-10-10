#!/usr/bin/env bash
# Runs the engine on this Mac as the voice assistant for an ESPHome satellite, with the settings in
# config.satellite.yaml (see the satellite section of the README). The device takes one engine at a
# time: scale a deployed engine to zero first. Ctrl-C stops it.
set -euo pipefail

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi
KEY_FILE=${VOICE_SATELLITE_KEY_FILE:-$HOME/.config/innestemme/satellite.key}
[ -z "${VOICE_SATELLITE_KEY:-}" ] && [ -f "$KEY_FILE" ] && export VOICE_SATELLITE_KEY_FILE=$KEY_FILE

CONFIG=${VOICE_CONFIG:-config.satellite.yaml}
[ -f "$CONFIG" ] || { echo "no $CONFIG; see the satellite section of the README" >&2; exit 1; }
LOG=${LOG:-target/satellite.log}
mkdir -p target/utterances

"${BIN:-target/release}/voice-engine" --stt-gpu --config "$CONFIG" "$@" 2>&1 | tee "$LOG"
