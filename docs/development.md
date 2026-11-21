# Development

```
make test     # cargo test --workspace
make lint     # cargo clippy --workspace --all-targets -- -D warnings
rustfmt --edition 2021 <changed files>   # a plain `cargo fmt` also rewrites a few hand-formatted spots
```

The toolchain is pinned in `rust-toolchain.toml`. `crates/voice-engine/tests/loopback.rs` streams through a real
server with 5% loss, reordering and a duplicate, under an allocator that aborts on any allocation in a guarded
section. The catalogue's tests parse every example phrase, so the web page cannot promise a phrase the rules do
not take.

## The assistant on the Mac

```
make setup        # once: release build with Metal, downloads the models
make kokoro       # Kokoro in Docker on 127.0.0.1:8880, for english-tts: kokoro
make assistant    # wait for "Talk now" and the chime, then talk; Ctrl-C stops
```

`make assistant` runs `voice-engine --processor assistant --stt-gpu` in the background and `voice-client` on the
microphone in the foreground. Settings go in `config.local.yaml` (git-ignored; copy `config.example.yaml`), Home
Assistant's address and token in `.env`:

```
HA_URL=http://homeassistant.local:8123
HA_TOKEN=<long-lived access token: HA > your profile > Security > Long-lived access tokens>
```

On the Mac, requests start with the name ("Homie, what's the weather?", `wake-name`), or the name alone gives a
chime and the next sentence is the request. Variables override single
settings for one run: `WAKE_NAME`, `LISTENER=whisper|parakeet`, `NORWEGIAN=true`, `ENGLISH_TTS`, `THREADS`,
`SPEAKER` (the Music Assistant player), `HOME_PLACE`, `DUMP=1` (every utterance to `target/utterances/`),
`SECONDS_LIVE`. `make log` follows what it heard and answered.

Other local services: `make piper` (Piper over Wyoming on 127.0.0.1:10200, for Norwegian or `english-tts:
piper`), `make ollama` (Qwen3 4B instruct for `llm-url`).

## Against the real Voice PE

Scale the cluster's engine to zero first, then `make satellite` with the cluster's settings in
`config.satellite.yaml`; see [deployment.md](deployment.md). The log is in `target/satellite.log` and the
recordings in `target/utterances`. Say so before anything plays on the real speakers.

## Tools

| Command | What it does |
|---|---|
| `cargo run -p voice-assistant --example home -- "what's on the shopping list"` | one home request through the rules and Home Assistant, without audio; scenes, scripts, light levels, phones and the TV only act with `RUN=1`, shopping list changes always do |
| `cargo run --release -p voice-assistant --example llm_eval -- <url> <model>` | the language model's accuracy and latency on fixed cases |
| `cargo run --release -p voice-assistant --example wake_score -- hey_jarvis target/utterances/run-*.wav` | openWakeWord's score on recordings, to pick `wake-threshold` |
| `cargo run --release -p voice-assistant --example wake_text -- hey_jarvis target/utterances/run-*.wav` | what Parakeet hears in each wake word |
| `cargo run --release -p voice-assistant --example stt_eval -- target/utterances/utterance-*.wav` | Whisper's transcripts and times; `stt_score` scores lists against a reference |
| `PHONON=Hugh cargo run --release -p voice-assistant --example kokoro_speed -- 4 "" <out dir>` | Kokoro in the engine against Phonon (and Kokoro-FastAPI with its URL), first audio and whole sentence, with wavs to compare by ear |
| `cargo run -p voice-assistant --example music -- "play my liked songs"` | a music request through Music Assistant |
| `cargo run -p voice-assistant --example speak -- 127.0.0.1:10200 <voice> out.wav "Hei"` | a Piper sample |
| `cargo run -p voice-esphome --example fake_satellite -- ...` | a pretend Voice PE, see [voice-pe.md](voice-pe.md) |
| `cargo run --release -p voice-engine --example party -- normal 60` | party mode from the Mac's microphone (Hue keys in `.env`; `normal`, `fast` or `relaxed`) |
| `scripts/latency-bench/` | end of speech to answer, a build against `main`, with a fake Home Assistant ([benchmarks.md](benchmarks.md)) |
| `scripts/replay/` | a build on a real day of Voice PE runs, live vs build per request (below) |
| `scripts/testset/` | the live recordings as a test set for speech models, with reference transcripts (below) |
| `voice-bench` | per-frame step times of the audio path ([benchmarks.md](benchmarks.md)) |

### The latency bench

```
bash scripts/latency-bench/wavs.sh target/utterances/<a run with a clear "Hey Jarvis">.wav <start s> <end s>
bash scripts/latency-bench/run.sh main <voice-engine built from main> 4
bash scripts/latency-bench/run.sh branch target/release/voice-engine 4
python3 -I scripts/latency-bench/compare.py target/latency-bench/main target/latency-bench/branch
```

It needs `make kokoro` and `cargo build --release -p voice-esphome --example fake_satellite`. The fake Home
Assistant reads from the real one in `.env` and never makes a service call. The engine runs with an empty
environment (only `PATH`, `HOME`, `RUST_LOG` and the token), so no `HA_URL`, `VOICE_*` or Hue value from `.env`
or the shell wins over `scripts/latency-bench/bench.yaml`. A rendered speech cache in
`target/latency-bench/seed` is copied into each run; flags after the rounds go to the engine
(`--english-tts kokoro-onnx`), and `QUESTIONS_DIR` points at other questions.

### Replaying a real day

The cluster's engine keeps each Voice PE run as a wav (`dump-utterances`, on its own backed-up volume at
`/recordings`), the last 365 days of them counted from the newest (`dump-keep-days`), so time away keeps the
days before it. A
replay runs a build on one day of them and compares it with what the live engine did:

```
kubectl --context admin@genesis -n monitoring port-forward svc/loki 3100:3100 &
bash scripts/replay/fetch.sh 2026-11-25                    # the day's runs and log lines, Oslo time
bash scripts/replay/run.sh main <voice-engine built from main> 2026-11-25
bash scripts/replay/run.sh branch target/release/voice-engine 2026-11-25 [engine flags]
open target/replay/branch/report.html
```

`fetch.sh` reads the files through a debug container, since the image has no shell. `run.sh` streams the day's
runs into the engine as one microphone, with the real gaps capped at 30 s, through `fake_satellite --continuous`;
answers go to an announce player on the fake Home Assistant (`scripts/latency-bench/fake_ha.py`), as in the
cluster, so nothing plays and no service call is made. The engine runs with an empty environment and
`scripts/replay/replay.yaml`. The report marks each run: `worse` (the build fails where live did not, or reaches
another skill), `better`, `changed` (other words, same outcome) or `same`; a live request repeated within 10 s
counts as failed. Replay `main` first: it should be close to all `same`, and the rest shows what a replay
cannot reproduce. Limits: timings are this Mac's; Music Assistant, Hue and Plex are not there; the house is as
it is now, not as it was; the clock, weather and prices are today's; wake words the live engine missed were never
recorded. Runs reuse one speech cache, `target/replay/cache/<voice>-sherpa-<version>`, so only the first fills it (the
common sentences first; `run.sh` waits at most `CACHE_WAIT`, 600 s). A new voice or sherpa-onnx version gets
a cache of its own; another change to how speech sounds gets one with `CACHE=<name>`.

To see what a branch changes, compare its replay with a replay of `main` on the same day, not with live:

```
python3 -I scripts/replay/report.py target/replay/days/2026-11-25 target/replay/branch --against target/replay/main
```

Both sides then ran on this Mac against the same house, so what live does differently (the clock, the house
as it was, the cluster's timings) drops out. The report also compares the Home Assistant service calls each
replay's fake logged (`ha.tsv`, sorted per run, bodies cut at 120 characters): a run whose calls differ is
marked `changed` at least, and the calls of each such run are printed. A replay of `main` against itself
should be all `same` with no calls that differ; answers with the time of day are the exception.

### The golden intent corpus

The text rules have a check of their own that covers more requests than a day of audio: every text the live
engine heard (its `heard` lines in Loki, as far back as Loki keeps them), plus every catalogue example, goes
through `intent::parse` on two builds, and the `Debug` output must be the same.

```
kubectl --context admin@genesis -n monitoring port-forward svc/loki 3100:3100 &
bash scripts/corpus/fetch.sh          # target/corpus/heard.txt, deduplicated; [days back, default 90]
bash scripts/corpus/compare.sh        # origin/main against the working tree; or <base ref> [other ref]
```

`examples/intent_corpus.rs` prints, for each text, the request as the worker passes it on (the name stripped
from its start and end), `intent::parse`, `parse_without_notes` where it differs, the parts of
`split_commands` and the catalogue skill. `compare.sh` builds it on a git worktree of the base ref under
`target/corpus/` (with this tree's copy of the tool) and on the working tree, and diffs the two outputs into
`target/corpus/diff.txt`. The texts are household speech: they stay in `target/`, which git ignores.

### The test set

The same recordings are the test set for a new listener, end-of-turn model or other speech change:

```
bash scripts/testset/sync.sh     # new runs and utterances to target/testset/
bash scripts/testset/label.sh    # reference.tsv for new utterances: Whisper large-v3-turbo on Metal
cargo run --release -p voice-assistant --example stt_eval -- target/testset/utterances/*.wav > whisper.tsv
cargo run -p voice-assistant --example stt_score -- target/testset/reference.tsv whisper.tsv
cargo run --release -p voice-assistant --example end_turn -- Jarvis target/testset/runs/run-*.wav   # pauses: end or mid
```

`runs/` holds each run at 16 kHz from the wake word to the end of the answer, as the device heard it (the
answer from the speaker is in it); `utterances/` holds each request as the engine cut it, at 24 kHz. The
reference is a machine transcript: right for clear requests, noisy for room talk and the TV, so check by ear
where a result turns on a few files. The runs from before the move on 2026-11-21 are in `/models/recordings`
(`REC=/proc/1/root/models/recordings bash scripts/testset/sync.sh`).

## The audio path on its own

```
cargo build --release -p voice-engine -p voice-client
./target/release/voice-engine --processor mimi --threads 4 --bind 127.0.0.1:7000 --metrics-bind 127.0.0.1:9090
./target/release/voice-client --tone 4                              # 440 Hz for 4 s
./target/release/voice-client --wav-in in.wav --wav-out out.wav     # 24 kHz mono s16
./target/release/voice-client --marker 20                           # mouth-to-ear delay from 20 tone bursts
./target/release/voice-client --seconds 10                          # live microphone, wear headphones
```

`--processor mimi` runs Kyutai's Mimi codec over every frame (its weights, about 385 MB, come from the Hugging
Face cache); `--processor passthrough` echoes without a model. `--marker` reports the delay a listener would hear
without the audio devices; `--seconds` reports mouth-to-ear with them, from cpal's capture and playback
timestamps. Make a test file with `say -o in.aiff "some words" && afconvert -f WAVE -d LEI16@24000 -c 1 in.aiff
in.wav`.

## Platform notes

- x86-64 Linux builds use `-C target-cpu=x86-64-v3` (`.cargo/config.toml`): candle picks its SIMD kernels at
  compile time, so a baseline build silently runs scalar code.
- aarch64 Linux (Docker on Apple Silicon) needs `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C
  target-feature=+fp16"`, otherwise `gemm-f16` fails to assemble.
- `voice-client` needs ALSA headers on Linux (`libasound2-dev`, `pkg-config`); the server does not.
