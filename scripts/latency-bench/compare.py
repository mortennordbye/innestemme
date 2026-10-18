"""Compares bench passes: from the end of speech to the first audio of the answer, per request.

    python3 -I scripts/latency-bench/compare.py target/latency-bench/main target/latency-bench/branch
"""

import re
import statistics
import sys
from collections import defaultdict


def ms(value):
    number, unit = re.match(r"([\d.]+)(ns|µs|ms|s)", value).groups()
    return float(number) * {"ns": 1e-6, "µs": 1e-3, "ms": 1, "s": 1000}[unit]


def runs(path):
    """(request, after_speech ms, answer) per answered request, in order."""
    out, heard, answer = [], None, None
    for line in open(f"{path}/engine.log", errors="replace"):
        if m := re.search(r'answering request="([^"]*)" answer="([^"]*)"', line):
            heard, answer = m[1], m[2]
        elif m := re.search(r"spoken first_audio_after=\S+ after_speech=Some\(([^)]+)\)", line):
            if heard:
                out.append((heard, ms(m[1]), answer))
            heard = None
    return out


passes = {path.rstrip("/").split("/")[-1]: runs(path) for path in sys.argv[1:]}
names = list(passes)
table = defaultdict(lambda: defaultdict(list))
for name, rows in passes.items():
    # The first round renders what the seed cache lacked; the rest are like production.
    seen = set()
    for request, after, answer in rows:
        key = request.rstrip(".?!").lower()
        if key in seen:
            table[key][name].append(after)
        seen.add(key)
        table[key].setdefault("answer", []).append(answer)

print(f"{'request':44}" + "".join(f"{n:>10}" for n in names) + "    rounds   answer")
totals = defaultdict(list)
for key, cols in table.items():
    meds = []
    for n in names:
        values = cols.get(n, [])
        meds.append(statistics.median(values) if values else float("nan"))
        totals[n].extend(values)
    counts = "/".join(str(len(cols.get(n, []))) for n in names)
    print(f"{key[:44]:44}" + "".join(f"{m:10.0f}" for m in meds) + f"    {counts:8} {cols['answer'][-1][:50]}")
print(f"{'median of all':44}" + "".join(f"{statistics.median(totals[n]) if totals[n] else float('nan'):10.0f}" for n in names))
