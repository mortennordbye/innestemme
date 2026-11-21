"""One microphone stream from a day of run wavs, for the fake satellite's --continuous mode.

Each run goes in at its real gap after the one before, capped at 30 s: longer than any answer and
the follow-up window, so a follow-up still lands inside the window and a new wake word outside it.
runs.tsv gives each run's place in the stream and its live start and end (Unix ms).

    python3 -I stream.py <day dir> <out dir>
"""

import array
import glob
import os
import sys
import wave

RATE, MAX_GAP = 16_000, 30.0
day, out = sys.argv[1], sys.argv[2]
runs = sorted(glob.glob(f"{day}/runs/run-*.wav"), key=lambda p: int(os.path.basename(p)[4:-4]))
stream, rows, prev_end = array.array("h"), [], None
for path in runs:
    with wave.open(path) as w:
        if w.getframerate() != RATE or w.getnchannels() != 1:
            sys.exit(f"{path}: expected 16 kHz mono")
        audio = array.array("h", w.readframes(w.getnframes()))
    # The file is saved when the run ends.
    end = int(os.path.basename(path)[4:-4])
    start = end - len(audio) * 1000 // RATE
    if prev_end is not None:
        gap = min(max((start - prev_end) / 1000, 0.0), MAX_GAP)
        stream.extend([0] * int(gap * RATE))
    else:
        stream.extend([0] * RATE)
    rows.append((os.path.basename(path), start, end, len(stream) / RATE, len(audio) / RATE))
    stream.extend(audio)
    prev_end = end
os.makedirs(out, exist_ok=True)
with wave.open(f"{out}/stream.wav", "wb") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(RATE)
    w.writeframes(stream.tobytes())
with open(f"{out}/runs.tsv", "w") as f:
    f.write("file\tlive_start_ms\tlive_end_ms\toffset_s\tseconds\n")
    for name, start, end, offset, seconds in rows:
        f.write(f"{name}\t{start}\t{end}\t{offset:.3f}\t{seconds:.3f}\n")
print(f"{len(rows)} runs, {len(stream) / RATE / 60:.1f} min of stream")
