"""The heard texts in one Loki query_range response (stdin), as `lang<TAB>text` lines.

Only the worker's "heard" lines count: a wake word check (`voice_engine::assistant::wake`) is not a request.
"""

import json
import re
import sys

ANSI = re.compile(r"\x1b\[[0-9;]*m")
HEARD = " voice_engine::assistant: heard text="


def field(text, name):
    m = re.search(rf'\b{name}=("((?:[^"\\]|\\.)*)"|\S+)', text)
    if not m:
        return None
    return re.sub(r"\\(.)", r"\1", m[2]) if m[2] is not None else m[1]


lines = [v[1] for s in json.load(sys.stdin)["data"]["result"] for v in s["values"]]
if len(lines) >= 5000:
    print("warning: 5000 lines in one window, some are missing", file=sys.stderr)
for line in lines:
    line = ANSI.sub("", line)
    if HEARD not in line:
        continue
    text = " ".join((field(line, "text") or "").split())
    if text:
        print(f"{field(line, 'lang') or 'English'}\t{text}")
