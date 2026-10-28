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
| `cargo run --release -p voice-assistant --example kokoro_speed -- 4 http://127.0.0.1:8880 <out dir>` | Kokoro in the engine against Kokoro-FastAPI, per sentence, with wavs to compare by ear |
| `cargo run -p voice-assistant --example music -- "play my liked songs"` | a music request through Music Assistant |
| `cargo run -p voice-assistant --example speak -- 127.0.0.1:10200 <voice> out.wav "Hei"` | a Piper sample |
| `cargo run -p voice-esphome --example fake_satellite -- ...` | a pretend Voice PE, see [voice-pe.md](voice-pe.md) |
| `cargo run --release -p voice-engine --example party -- normal 60` | party mode from the Mac's microphone (Hue keys in `.env`; `normal`, `fast` or `relaxed`) |
| `scripts/latency-bench/` | end of speech to answer, a build against `main`, with a fake Home Assistant ([benchmarks.md](benchmarks.md)) |
| `voice-bench` | per-frame step times of the audio path ([benchmarks.md](benchmarks.md)) |

### The latency bench

```
bash scripts/latency-bench/wavs.sh target/utterances/<a run with a clear "Hey Jarvis">.wav <start s> <end s>
bash scripts/latency-bench/run.sh main <voice-engine built from main> 4
bash scripts/latency-bench/run.sh branch target/release/voice-engine 4
python3 -I scripts/latency-bench/compare.py target/latency-bench/main target/latency-bench/branch
```

It needs `make kokoro` and `cargo build --release -p voice-esphome --example fake_satellite`. The fake Home
Assistant reads from the real one in `.env` and never makes a service call; `run.sh` passes `--ha-url` because
`HA_URL` from `.env` would otherwise win over the bench's settings file. A rendered speech cache in
`target/latency-bench/seed` is copied into each run; flags after the rounds go to the engine
(`--english-tts kokoro-onnx`), and `QUESTIONS_DIR` points at other questions.

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
