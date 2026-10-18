#!/usr/bin/env bash
# The bench's questions: a real "Hey Jarvis" from a Voice PE recording, then a request in a macOS voice.
# wavs.sh <16 kHz run wav> <start s> <end s>, e.g. a run where `examples/wake_score.rs` scores high.
set -euo pipefail
OUT=target/latency-bench/wav; mkdir -p "$OUT"
i=0
while IFS= read -r request; do
  i=$((i + 1))
  say -v Samantha -o "$OUT/request$i.wav" --data-format=LEI16@16000 "$request"
done <<'REQUESTS'
Turn off the kitchen lights.
Initiate lockdown.
Stand down.
Where is my phone?
Put the weather on the TV.
What's the temperature in the living room?
Set a timer for five minutes.
Good night.
REQUESTS
python3 -I - "$1" "$2" "$3" "$OUT" <<'PY'
import array, glob, sys, wave
run, start, end, out = sys.argv[1], float(sys.argv[2]), float(sys.argv[3]), sys.argv[4]
w = wave.open(run)
audio = array.array("h", w.readframes(w.getnframes()))
wake = audio[int(start * 16000):int(end * 16000)]
silence = lambda s: array.array("h", [0] * int(s * 16000))
for path in sorted(glob.glob(f"{out}/request*.wav")):
    r = wave.open(path)
    request = array.array("h", r.readframes(r.getnframes()))
    o = wave.open(path.replace("request", "question"), "wb")
    o.setnchannels(1), o.setsampwidth(2), o.setframerate(16000)
    o.writeframes((silence(0.5) + wake + silence(0.4) + request + silence(4)).tobytes())
PY
ls "$OUT"/question*.wav
