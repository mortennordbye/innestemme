#!/usr/bin/env bash
# Runs the voice assistant locally: the engine in the background, the microphone client in the
# foreground, and a readable summary of the server log in between. Stopping the client (Ctrl-C or
# the timer) stops everything.
set -euo pipefail

# HA_URL and HA_TOKEN (Home Assistant, for lights) live in a git-ignored .env next to the Makefile.
if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

BIN=${BIN:-target/release}
# Personal settings (name, home, speaker, voice...) live in a settings file; see config.example.yaml.
# Environment variables below override single settings for one run.
CONFIG=${VOICE_CONFIG:-config.local.yaml}
[ -f "$CONFIG" ] || CONFIG=
NAME=${WAKE_NAME:-}
if [ -z "$NAME" ] && [ -n "$CONFIG" ]; then
  NAME=$(sed -nE 's/^wake[-_]name:[[:space:]]*"?([^"#]*[^"#[:space:]])"?.*/\1/p' "$CONFIG" | head -1)
fi
NAME=${NAME:-Homie}
SECONDS_LIVE=${SECONDS_LIVE:-600}
ADDR=${ADDR:-127.0.0.1:7000}
METRICS=${METRICS:-127.0.0.1:9090}
LOG=${LOG:-target/assistant.log}

bold=$'\e[1m' green=$'\e[32m' cyan=$'\e[36m' dim=$'\e[2m' yellow=$'\e[33m' reset=$'\e[0m'

"$BIN/voice-engine" --processor assistant --stt-gpu --bind "$ADDR" --metrics-bind "$METRICS" \
  ${CONFIG:+--config "$CONFIG"} \
  ${WAKE_NAME:+--wake-name "$WAKE_NAME"} ${SPEAKER:+--speaker "$SPEAKER"} ${NORWEGIAN:+--norwegian "$NORWEGIAN"} \
  ${LISTENER:+--listener "$LISTENER"} ${WHISPER:+--whisper-size "$WHISPER"} \
  ${ENGLISH_TTS:+--english-tts "$ENGLISH_TTS"} ${VOICE:+--pocket-voice "$VOICE"} \
  ${POCKET_PRECISION:+--pocket-precision "$POCKET_PRECISION"} ${THREADS:+--threads "$THREADS"} \
  ${DUMP:+--dump-utterances target/utterances} \
  ${HOME_PLACE:+--home "$HOME_PLACE"} ${SAY_VOICE:+--say-voice "$SAY_VOICE"} \
  ${SAY_VOICE_NO:+--say-voice-no "$SAY_VOICE_NO"} >"$LOG" 2>&1 &
engine=$!
watcher=
cleanup() {
  code=$?
  # SIGPIPE to `tail` only: the reader loop ends on EOF, and bash does not report SIGPIPE deaths.
  [ -n "$watcher" ] && pkill -PIPE -P "$watcher" -x tail 2>/dev/null || true
  kill $engine 2>/dev/null
  wait $engine 2>/dev/null || true
  exit $code
}
trap cleanup EXIT

printf '%sLoading models, wait for the "Talk now" line and the chime...%s\n' "$dim" "$reset"
for _ in $(seq 120); do
  curl -sf "http://$METRICS/healthz" >/dev/null && break
  kill -0 $engine 2>/dev/null || { echo "engine exited, see $LOG" >&2; tail -5 "$LOG" >&2; exit 1; }
  sleep 1
done

# What the assistant hears and says, from the server log.
(
  tail -n +1 -F "$LOG" 2>/dev/null | while IFS= read -r line; do
    case "$line" in
      *'home assistant connected'*)
        n=${line##*lights=}
        printf '%s● Home Assistant connected, %s lights.%s\n' "$green" "$n" "$reset" ;;
      *'home assistant not usable'*)
        printf '%s! Home Assistant not usable: %s%s\n' "$yellow" "${line##*error=}" "$reset" ;;
      *'music assistant connected'*)
        p=${line##*player=}
        printf '%s● Music Assistant connected, music plays on %s.%s\n' "$green" "${p//\"/}" "$reset" ;;
      *'music assistant not usable'*)
        printf '%s! Music Assistant not usable: %s%s\n' "$yellow" "${line##*error=}" "$reset" ;;
      *'assistant: ready'*) printf '%s%s● Ready (you heard a chime).%s\n' "$bold" "$green" "$reset" ;;
      *'assistant: heard text='*)
        heard=${line#*heard text=\"}; heard=${heard%%\" lang=*} ;;
      *'assistant: heard words='*)
        heard=${line#*heard words=\"}; heard=${heard%%\" deaf=*} ;;
      *'assistant: not addressed'*)
        printf '%s  (ignored, did not start with "%s": %s)%s\n' "$dim" "$NAME" "$heard" "$reset" ;;
      *'assistant: wake word'*)
        printf '  you:   %s\n%s  %s: (listening)%s\n' "$heard" "$cyan" "$NAME" "$reset" ;;
      *'assistant: answering'*)
        a=${line#*answer=\"}; a=${a%%\" lang=*}; a=${a//\\\"/\"}
        printf '  you:   %s\n%s  %s: %s%s\n' "$heard" "$cyan" "$NAME" "$a" "$reset" ;;
      *'assistant: spoken'*)
        printf '%s         (still listening for a few seconds, no need to say "%s")%s\n' "$dim" "$NAME" "$reset" ;;
      *'conversation ended'*)
        printf '  you:   %s\n%s  %s: (okay)%s\n' "$heard" "$cyan" "$NAME" "$reset" ;;
      *' WARN '*|*' ERROR '*) printf '%s  ! %s%s\n' "$yellow" "${line#*Z }" "$reset" ;;
    esac
  done
) &
watcher=$!

printf '\n%s%sSay "%s", then ask: the weather, a joke, the lights or music.%s\n' "$bold" "$green" "$NAME" "$reset"
printf '%s  e.g. "%s, what is the weather in Oslo?", "%s, tell me a joke",%s\n' "$dim" "$NAME" "$NAME" "$reset"
printf '%s       "%s, turn off the light in the living room",%s\n' "$dim" "$NAME" "$reset"
printf '%s       "%s, play my liked songs", "%s, play Careless Whisper", "%s, next song"%s\n' "$dim" "$NAME" "$NAME" "$NAME" "$reset"
[ -z "$CONFIG" ] && printf '%s  (no config.local.yaml: copy config.example.yaml to set your name, home, speaker)%s\n' "$yellow" "$reset"
[ -z "${HA_TOKEN:-}" ] && printf '%s  (lights need HA_URL and HA_TOKEN in .env)%s\n' "$yellow" "$reset"
printf '%s  Full server log: %s%s\n\n' "$dim" "$LOG" "$reset"
"$BIN/voice-client" --server "$ADDR" --seconds "$SECONDS_LIVE"
