#!/usr/bin/env bash
# Builds the Home Assistant Voice PE firmware with firmware/voice-pe.patch, and flashes it over the
# network. `scripts/firmware.sh build`, or `DEVICE=<ip> scripts/firmware.sh flash` (builds first).
# Wi-Fi and the API encryption key stay on the device: the firmware sets neither, like the official one.
set -euo pipefail

REF=${FIRMWARE_REF:-26.9.0}
ESPHOME_IMAGE=${ESPHOME_IMAGE:-ghcr.io/esphome/esphome:2026.6.0}
DIR=target/firmware/$REF
CONFIG=home-assistant-voice.factory.yaml

if [ ! -d "$DIR/.git" ]; then
  git clone --quiet --depth 1 --branch "$REF" https://github.com/esphome/home-assistant-voice-pe.git "$DIR"
fi
git -C "$DIR" checkout --quiet .
git -C "$DIR" apply "$PWD/firmware/voice-pe.patch"

esphome() {
  # The toolchain (~1 GB) lives in a volume, so only the first build downloads it.
  docker run --rm -v "$PWD/$DIR:/config" -v innestemme-platformio:/root/.platformio "$ESPHOME_IMAGE" "$@"
}

case "${1:-build}" in
  build) esphome compile "$CONFIG" ;;
  flash)
    [ -n "${DEVICE:-}" ] || { echo "set DEVICE to the Voice PE's address" >&2; exit 1; }
    esphome run "$CONFIG" --device "$DEVICE" --no-logs
    ;;
  *) echo "usage: $0 build|flash" >&2; exit 1 ;;
esac
