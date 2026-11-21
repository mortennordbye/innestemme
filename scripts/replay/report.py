"""Live vs candidate per run of a replayed day: what each heard, the skill it reached, the answer and
the time from the end of speech to the first audio. Writes report.html and report.tsv next to the run.

    python3 -I report.py target/replay/days/<day> target/replay/<name>
    python3 -I report.py target/replay/days/<day> target/replay/<name> --against target/replay/<other>

With --against, the other side is another replay of the same day (main against a branch) instead of
the live log, and the Home Assistant service calls each replay's fake made (ha.tsv) are compared too:
a run whose calls differ is at least "changed". The repeat check is for live runs only.

A run failed when it ended unhandled, not understood, as "not speech", with no request after a named
wake word, or with an apology. On the live side a run also failed when the next run starts within
10 s and says much the same: the request was repeated.
"""

import html
import os
import re
import statistics
import sys
from datetime import datetime

day, out = sys.argv[1], sys.argv[2]
against = sys.argv[4] if len(sys.argv) > 4 and sys.argv[3] == "--against" else None
ANSI = re.compile(r"\x1b\[[0-9;]*m")
LINE = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)\s+\w+\s+[\w:]+: (.*)$")
REPEAT_S = 10


def ms(value):
    number, unit = re.match(r"([\d.]+)(ns|µs|us|ms|s)", value).groups()
    return float(number) * {"ns": 1e-6, "µs": 1e-3, "us": 1e-3, "ms": 1, "s": 1000}[unit]


def field(text, name):
    m = re.search(rf'\b{name}=("((?:[^"\\]|\\.)*)"|\S+)', text)
    if not m:
        return None
    return m[2].replace('\\"', '"') if m[2] is not None else m[1]


def lines(path):
    """(Unix ms, message) per log line."""
    found = []
    for raw in open(path, errors="replace"):
        m = LINE.match(ANSI.sub("", raw.rstrip("\n")))
        if m:
            stamp = datetime.fromisoformat(m[1].replace("Z", "+00:00")).timestamp() * 1000
            found.append((stamp, m[2]))
    return found


def outcome(segment):
    """What one run came to, from its log lines."""
    o = {"wake": "", "heard": [], "skill": "", "answer": "", "after_ms": None, "state": "nothing"}
    for _, text in segment:
        message = re.sub(r'\s\w+=("(?:[^"\\]|\\.)*"|\S+)', "", text).strip()
        if message.startswith("wake word confirmed by its name"):
            o["wake"] = "name check"
        elif message.startswith("no name in the wake word"):
            o["wake"] = o["wake"] or "no name"
            o["state"] = "ignored" if o["state"] == "nothing" else o["state"]
        elif message == "wake word" and not o["wake"]:
            o["wake"] = f"score {field(text, 'score')}"
        elif message == "heard":
            o["heard"].append(field(text, "text") or "")
        elif message in ("answering", "done without words") and not o["skill"]:
            o["skill"] = field(text, "skill") or "?"
            o["answer"] = field(text, "answer") or ""
            apology = o["answer"].startswith(("Sorry", "Beklager"))
            o["state"] = "unhandled" if o["answer"].startswith("Sorry, I can't help") else "apology" if apology else "answered"
        elif message == "spoken" and o["after_ms"] is None:
            after = field(text, "after_speech")
            m = re.match(r"Some\(([^)]+)\)", after or "")
            if m:
                o["after_ms"] = ms(m[1])
        elif o["state"] in ("nothing", "ignored"):
            for key, state in (
                ("not speech", "not speech"),
                ("not understood after the wake word", "not understood"),
                ("no request after the wake word", "no request"),
                ("not addressed", "ignored"),
                ("own answer heard", "ignored"),
                ("conversation ended", "ended"),
            ):
                if message.startswith(key):
                    o["state"] = state
    return o


FAILED = {"unhandled", "apology", "not speech", "not understood", "no request"}


def words(texts):
    return set(re.findall(r"[a-zæøå']+", " ".join(texts).lower())) - {"hey", "jarvis", "the", "a", "please"}


def similar(a, b):
    a, b = words(a), words(b)
    return bool(a and b) and len(a & b) / min(len(a), len(b)) >= 0.5


def segments(log, starts):
    """The log lines of each run: from its start to the next run's start."""
    bounds = starts + [starts[-1] + 120_000]
    return [[(t, m) for t, m in log if bounds[i] - 300 <= t < bounds[i + 1] - 300] for i in range(len(starts))]


def ha_calls(path, starts):
    """The fake Home Assistant's service calls in each run, sorted: calls written behind may land in
    either order. ha.tsv holds `start<TAB>end<TAB>domain.service<TAB>body`, the body over several lines."""
    calls = []
    for line in open(path) if os.path.exists(path) else []:
        m = re.match(r"(\d+\.\d+)\t[\d.]+\t(\S+)\t(.*)", line)
        if m:
            calls.append([float(m[1]) * 1000, m[2], m[3]])
        elif calls:
            calls[-1][2] += line
    bounds = starts + [starts[-1] + 120_000]
    return [
        sorted(f"{s} {' '.join(body.split())}" for t, s, body in calls if bounds[i] - 300 <= t < bounds[i + 1] - 300)
        for i in range(len(starts))
    ]


def stream_starts(replay, runs):
    """Where each run starts on a replay's clock (Unix ms)."""
    log = open(f"{replay}/satellite.txt")
    start = next(int(line.split()[0]) for line in log if re.match(r"\d+ stream start", line))
    return [start + int(r["offset"] * 1000) for r in runs]


rows = [line.rstrip("\n").split("\t") for line in open(f"{out}/runs.tsv")][1:]
runs = [{"file": r[0], "start": int(r[1]), "end": int(r[2]), "offset": float(r[3])} for r in rows]
cand_starts = stream_starts(out, runs)
cand = segments(lines(f"{out}/engine.log"), cand_starts)
if against:
    other = [line.split("\t")[0] for line in open(f"{against}/runs.tsv")][1:]
    if other != [r["file"] for r in runs]:
        sys.exit(f"{against} replayed other runs than {out}")
    live_starts = stream_starts(against, runs)
    live = segments(lines(f"{against}/engine.log"), live_starts)
    ha_live, ha_cand = ha_calls(f"{against}/ha.tsv", live_starts), ha_calls(f"{out}/ha.tsv", cand_starts)
else:
    live = segments(lines(f"{day}/live.log"), [r["start"] for r in runs])
for i, run in enumerate(runs):
    run["live"], run["cand"] = outcome(live[i]), outcome(cand[i])
    if against:
        run["live"]["ha"], run["cand"]["ha"] = ha_live[i], ha_cand[i]
for i, run in enumerate([] if against else runs[:-1]):
    nxt = runs[i + 1]
    if nxt["start"] - run["end"] <= REPEAT_S * 1000 and similar(run["live"]["heard"], nxt["live"]["heard"]):
        run["live"]["repeated"] = True


def failed(o):
    return o["state"] in FAILED or o.get("repeated", False)


def mark(run):
    a, b = run["live"], run["cand"]
    if failed(a) and not failed(b) and b["state"] == "answered":
        return "better"
    if failed(b) and not failed(a):
        return "worse"
    if a["state"] == b["state"] == "answered" and a["skill"] != b["skill"]:
        return "worse"
    if a.get("ha") != b.get("ha"):
        return "changed"
    if (a["state"], a["skill"], a["heard"], a["answer"]) == (b["state"], b["skill"], b["heard"], b["answer"]):
        return "same"
    return "changed"


for run in runs:
    run["mark"] = mark(run)
counts = {m: sum(r["mark"] == m for r in runs) for m in ("worse", "better", "changed", "same")}
requests = sum(bool(r["live"]["heard"] or r["cand"]["heard"]) for r in runs)
pairs = [(r["live"]["after_ms"], r["cand"]["after_ms"]) for r in runs if r["live"]["after_ms"] and r["cand"]["after_ms"]]
p50 = lambda xs: f"{statistics.median(xs):.0f}" if xs else "-"
name = os.path.basename(out.rstrip("/"))
base, where = (os.path.basename(against.rstrip("/")), "this Mac") if against else ("live", "cluster")
timing = f"after speech p50, runs both answered: {base} {p50([a for a, _ in pairs])} ms ({where}), candidate {p50([b for _, b in pairs])} ms (this Mac)"
day_name = os.path.basename(day.rstrip("/"))

with open(f"{out}/report.tsv", "w") as f:
    side = ["state", "skill", "heard", "answer", "ms"] + (["ha"] if against else [])
    f.write("\t".join(["mark", "time", "file"] + [f"{p}_{c}" for p in ("live", "cand") for c in side]) + "\n")
    for r in runs:
        cols = [r["mark"], datetime.fromtimestamp(r["start"] / 1000).strftime("%H:%M:%S"), r["file"]]
        for o in (r["live"], r["cand"]):
            state = o["state"] + (" repeated" if o.get("repeated") else "")
            cols += [state, o["skill"], " | ".join(o["heard"]), o["answer"], f"{o['after_ms']:.0f}" if o["after_ms"] else ""]
            cols += [" | ".join(o["ha"])] if against else []
        f.write("\t".join(c.replace("\t", " ") for c in cols) + "\n")

e = html.escape


def cell(o):
    heard = " / ".join(f"“{e(h)}”" for h in o["heard"]) or f"<i>{e(o['wake'] or 'no wake')}</i>"
    state = o["skill"] if o["state"] == "answered" else o["state"]
    extra = '<div class="note">repeated within 10 s</div>' if o.get("repeated") else ""
    answer = f'<div class="note">{e(o["answer"])}</div>' if o["answer"] else ""
    calls = "".join(f'<div class="note">HA: {e(c)}</div>' for c in o.get("ha", []))
    return f"<td>{heard} → <b>{e(state)}</b>{answer}{extra}{calls}</td>"


order = {"worse": 0, "better": 1, "changed": 2, "same": 3}
body = []
for r in sorted(runs, key=lambda r: (order[r["mark"]], r["start"])):
    a, b = r["live"]["after_ms"], r["cand"]["after_ms"]
    t = f"{a:.0f}" if a else "-", f"{b:.0f}" if b else "-"
    body.append(
        f'<tr data-mark="{r["mark"]}"><td class="t">{datetime.fromtimestamp(r["start"] / 1000):%H:%M:%S}</td>'
        f'<td><audio controls preload="none" src="../days/{e(day_name)}/runs/{e(r["file"])}"></audio></td>'
        f'{cell(r["live"])}{cell(r["cand"])}<td class="t">{t[0]} / {t[1]}</td>'
        f'<td><span class="pill {r["mark"]}">{r["mark"]}</span></td></tr>'
    )
buttons = "".join(f'<button data-f="{m}">{m} {counts[m]}</button>' for m in counts)
page = f"""<!doctype html><meta charset="utf-8"><title>Replay {e(day_name)}: {e(name)} vs {e(base)}</title>
<style>
:root {{ --bg:#f6f7f9; --fg:#1b1f27; --muted:#5b6474; --line:#dde1e8; --surface:#fff;
  --good:#1f7a4d; --good-bg:#e6f4ec; --bad:#b4372f; --bad-bg:#fbe9e7; --warn:#9a6a00; --warn-bg:#fbf2dc }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#12151b; --fg:#e4e7ec; --muted:#9aa3b2; --line:#2c323d;
  --surface:#1a1e26; --good:#5fcf94; --good-bg:#173326; --bad:#ff8a80; --bad-bg:#3a1d1b; --warn:#e9c15a;
  --warn-bg:#352b12; color-scheme: dark }} }}
body {{ background:var(--bg); color:var(--fg); font:14px/1.45 system-ui, sans-serif; margin:0; padding:24px 16px }}
h1 {{ font-size:20px; margin:0 0 8px }} .sub {{ color:var(--muted); margin:0 0 12px }}
.bar {{ display:flex; flex-wrap:wrap; gap:8px; margin-bottom:12px }}
button {{ font:inherit; border:1px solid var(--line); background:var(--surface); color:var(--fg); border-radius:4px; padding:4px 10px; cursor:pointer }}
button.on {{ border-color:var(--fg) }}
.wrap {{ overflow-x:auto; background:var(--surface); border:1px solid var(--line); border-radius:4px }}
table {{ border-collapse:collapse; width:100%; min-width:900px }}
th, td {{ text-align:left; padding:7px 10px; border-bottom:1px solid var(--line); vertical-align:top }}
th {{ font-size:11px; text-transform:uppercase; letter-spacing:.06em; color:var(--muted); font-weight:500 }}
td.t {{ font-family:ui-monospace, monospace; font-variant-numeric:tabular-nums; white-space:nowrap }}
audio {{ width:180px; height:28px }} .note {{ color:var(--muted); font-size:12px }}
.pill {{ font-size:11px; padding:1px 7px; border-radius:10px; font-weight:500 }}
.better {{ background:var(--good-bg); color:var(--good) }} .worse {{ background:var(--bad-bg); color:var(--bad) }}
.changed {{ background:var(--warn-bg); color:var(--warn) }} .same {{ background:var(--bg); color:var(--muted) }}
</style>
<h1>Replay {e(day_name)}: {e(name)} vs {e(base)}</h1>
<p class="sub">{len(runs)} runs, {requests} with words heard. {e(timing)}.</p>
<div class="bar"><button data-f="" class="on">all {len(runs)}</button>{buttons}</div>
<div class="wrap"><table><thead><tr><th>Time</th><th>Audio</th><th>{e(base.capitalize())} heard → skill</th><th>Candidate heard → skill</th><th>After speech ms</th><th>Mark</th></tr></thead>
<tbody>{"".join(body)}</tbody></table></div>
<script>
document.querySelectorAll("button").forEach(b => b.onclick = () => {{
  document.querySelectorAll("button").forEach(x => x.classList.toggle("on", x === b));
  document.querySelectorAll("tbody tr").forEach(r => r.hidden = !!b.dataset.f && r.dataset.mark !== b.dataset.f);
}});
</script>
"""
open(f"{out}/report.html", "w").write(page)
print(f"{counts['worse']} worse, {counts['better']} better, {counts['changed']} changed, {counts['same']} same; {timing}")
if against:
    differ = [r for r in runs if r["live"]["ha"] != r["cand"]["ha"]]
    total = lambda side: sum(map(len, side))
    print(f"home assistant calls: {total(ha_live)} in {base}, {total(ha_cand)} in {name}; {len(differ)} runs differ")
    for r in differ:
        print(f"  {r['file']}\n    {base}: {r['live']['ha']}\n    {name}: {r['cand']['ha']}")
print(f"report: {out}/report.html")
