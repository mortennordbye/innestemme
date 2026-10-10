<div align="center">

# 🤫 innestemme

### A local voice assistant that replaces Home Assistant's Assist, on your own CPU.

[![Rust](https://img.shields.io/badge/Rust-000000?logo=rust&logoColor=white)](https://www.rust-lang.org) [![Home Assistant](https://img.shields.io/badge/Home%20Assistant-18BCF2?logo=homeassistant&logoColor=white)](https://www.home-assistant.io) [![ESPHome](https://img.shields.io/badge/ESPHome-000000?logo=esphome&logoColor=white)](https://esphome.io) [![Ollama](https://img.shields.io/badge/Ollama-000000?logo=ollama&logoColor=white)](https://ollama.com)

[![CI](https://github.com/mortennordbye/innestemme/actions/workflows/ci.yml/badge.svg)](https://github.com/mortennordbye/innestemme/actions/workflows/ci.yml) [![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/mortennordbye/innestemme/badge)](https://securityscorecards.dev/viewer/?uri=github.com/mortennordbye/innestemme)

[![License](https://img.shields.io/github/license/mortennordbye/innestemme?style=flat-square)](LICENSE) [![Last Commit](https://img.shields.io/github/last-commit/mortennordbye/innestemme?style=flat-square)](https://github.com/mortennordbye/innestemme/commits/main) [![Stars](https://img.shields.io/github/stars/mortennordbye/innestemme?style=flat-square)](https://github.com/mortennordbye/innestemme/stargazers)

*Innestemme* is Norwegian for "indoor voice": a voice assistant that never leaves the house. Speech recognition,
the language model and the voice all run locally, in English and Norwegian, and it talks to Home Assistant and
ESPHome satellites such as the Home Assistant Voice PE.

</div>

---

## Overview

Audio comes in over UDP (from the test client or an ESPHome satellite bridged by the engine), Whisper turns it
into text, rules handle the common requests and a small local language model (any OpenAI-compatible server, for
example Ollama with Qwen3 4B) handles the rest by picking a skill. Answers are spoken with Pocket TTS (English) or
Piper (Norwegian). The audio path does no heap allocation after startup, and every stage reports its latency.

| Skill | Examples |
|---|---|
| Lights | "turn off the kitchen lights", "dim the living room to 30%", "make the bedroom red", "which lights are on?" |
| Scenes and scripts | "set the living room to relax", "run movie time", or a script's name on its own |
| Shopping list | "add milk and eggs to the shopping list", "what's on the list?", "legg til melk på handlelista" |
| Music | "play my liked songs", "next song", "turn it down" (Music Assistant) |
| Timers and reminders | "set a pasta timer for 8 minutes", "remind me to call mum in an hour" |
| Weather | "what's the weather tomorrow?", "hvordan blir været i Bergen?" (Yr) |
| Departures | "when's the next bus?", "how do I get to Majorstuen?" (Entur) |
| Electricity prices | "when is power cheapest tonight?", "hva koster strømmen nå?" |
| Home | "who's home?", "good morning" (a short briefing) |
| Anything else | answered by the language model in one sentence |

## Layout

| crate | what it is |
|---|---|
| `voice-proto` | Wire format: 12-byte header, `Hello` / `Audio` / `Bye`, PCM helpers. |
| `voice-rt` | Real-time plumbing: reorder buffer, atomic histogram, allocation guard, pinned threads. |
| `voice-codec` | Packet codec (raw PCM) and `MimiCodec` over `moshi` / candle. |
| `voice-engine` | The server: UDP transport, net task and model thread joined by lock-free rings, `/metrics`. |
| `voice-assistant` | Assistant parts: Whisper and Kyutai STT, wake word and turn logic, intent rules, the language model client, skills (Home Assistant lights, scenes, shopping list, Music Assistant, timers, Yr, Entur, electricity prices), Pocket TTS and Piper output. |
| `voice-esphome` | ESPHome native API client (plaintext and Noise), so the engine can serve a Voice PE instead of Home Assistant's Assist. |
| `voice-client` | Test client: tone, wav file or live microphone. Reports frame turnaround and mouth-to-ear. |
| `voice-bench` | Per-frame step times for [docs/benchmarks.md](docs/benchmarks.md). |

## Run it

```
cargo build --release -p voice-engine -p voice-client

./target/release/voice-engine --processor mimi --threads 4 --bind 127.0.0.1:7000 --metrics-bind 127.0.0.1:9090
./target/release/voice-client --tone 4                              # 440 Hz for 4 s
./target/release/voice-client --wav-in in.wav --wav-out out.wav     # 24 kHz mono s16
./target/release/voice-client --marker 20                           # mouth-to-ear delay from 20 tone bursts
./target/release/voice-client --seconds 10                          # live microphone, wear headphones
curl -s 127.0.0.1:9090/metrics
```

### Local voice assistant (macOS)

Say "Homie" and ask about the weather, in English or Norwegian: "Homie, what's the weather in Oslo?", "Homie,
hvordan blir været i Bergen i morgen?". Saying just "Homie" gives a chime, and the next thing you say within 8 s
is the question. The forecast comes live from Yr (MET Norway); a place is looked up by name (a Norwegian place
of that name first, so "Bergen" is never somewhere abroad), and with `address` set the home's weather is for
exactly there. The answer is spoken in the language you used:
English with Kyutai Pocket TTS on the CPU (natural voice, streamed as it is generated), Norwegian with macOS
`say` (Nora) until there is a Norwegian Pocket TTS model. Answers are one short sentence ("It's 14 degrees and overcast in Oslo, with a high of 17 and rain is likely"),
mentioning rain only from 30% and wind only from 8 m/s.

Jokes: "Homie, tell me a joke" / "Hei Homie, fortell en vits". English jokes come live from icanhazdadjoke.com
(with a built-in fallback), Norwegian ones from a built-in list; the last 8 are not repeated. The service's
jokes are unfiltered dad jokes, and a few lean on stereotypes.

Departures from Entur (all of Norway, real time): "Homie, when's the next bus?", "next tram to Majorstuen",
"når går neste trikk?", "neste tog til Lillestrøm". Without a destination: the next departures from the stops
nearest `address` (or `transit-stops`) that can still be reached on foot, from the walk to each stop. With one:
a trip from home, as in [ruter-cli](https://github.com/mortennordbye/ruter-cli), which also finds lines that
only pass through the destination and says when to head out: "Tram 15 to Majorstuen leaves Øvre Slottsgate in
9 minutes; head out in 6 minutes." Misheard stop names are found by Entur's geocoder.

Timers and reminders: "Homie, set a timer for 10 minutes", "set a pasta timer for 8 minutes", "remind me in
an hour to call my mother", "how much time is left?", "pause / resume / cancel the timer", "add 5 minutes to
the timer", "cancel all timers"; Norwegian too ("sett en timer på ti minutter", "minn meg på å ringe mamma om
en time", "hvor lang tid er det igjen?"). Durations in digits or words ("twenty-five minutes", "an hour and a
half", "en halvtime", "tre kvarter"). "Set a timer" alone asks "For how long?". When a timer ends the engine
rings and says which one; a reminder speaks its message ("Reminder: call your mother."). Timers live in the
engine's memory and do not survive a restart.

A greeting before the name works ("Hi Freya", "Hei Freya", "Hei hei Freya", "God morgen Freya"), also when the
transcript glues them together ("Heifreya"). The terminal shows only what you said, what Freya answered, and a
dim line for speech that did not start with the name. Anything the rules do not recognise goes to the
language model when one is set, else gets a polite "only weather, jokes, lights, music and timers so far".

```
make setup        # once: release build with Metal, downloads the models (about 700 MB)
make assistant    # wait for "Talk now" and the chime, then talk; Ctrl-C stops
```

Personal settings (name, home place, speaker, voice) go in `config.local.yaml`; copy
`config.example.yaml` to start. The variables below override single settings for one run. The name
is a setting: `WAKE_NAME=Homie` (default). Any name works; known ones also accept common misspellings.
`DUMP=1 make assistant` saves every utterance to `target/utterances/` for tuning recognition on a real voice.

Conversation: for 8 s after each answer (and after a bare "Homie") you can go on without the name; only
recognised requests count then, so room chatter is ignored. "Turn them back on", "turn it off", "reverse that"
and "undo" refer to the lights switched in the last 2 minutes; "turn off the lights" with nothing to refer to
gets "Which room?" and the next answer is the room. "Thanks" gets "You're welcome"; "never mind" ends it.

Options for `make assistant`: `VOICE=cosette` (Pocket TTS preset, the default; others: alba, fantine, eponine, azelma, marius, javert, jean),
`POCKET_PRECISION=f32|q8`, `THREADS=3`, `ENGLISH_TTS=pocket|say`, `NORWEGIAN=false|true`,
`LISTENER=whisper|kyutai`, `WHISPER=base|small`
(small is more accurate, a 970 MB download on first use), `HOME_PLACE=Oslo`
(for questions that name no place), `SAY_VOICE=...`, `SAY_VOICE_NO=Nora`, `SECONDS_LIVE=600`. It runs
`voice-engine --processor assistant --stt-gpu` in the background and `voice-client` on the microphone in the
foreground, and stops both together. Headphones are optional: the microphone is ignored while a request is
handled, while the reply plays, and for a second after.

Lights through Home Assistant: "Homie, turn off the light in my living room", "Homie, turn on the kitchen
lights". Create `.env` next to the Makefile (git-ignored, read by `make assistant`):

```
HA_URL=http://homeassistant.local:8123
HA_TOKEN=<long-lived access token: HA > your profile > Security > Long-lived access tokens>
```

At start the engine lists every light with its area through HA's template API and prints how many it found. A
request names a room (every light in that area) or a light by name; room names match across English and
Norwegian ("living room" finds an area called "Stue"). Switching is one `light.turn_on`/`turn_off` call.

Music on the Sonos, from Spotify, through Music Assistant (the Home Assistant add-on): "Homie, play my liked
songs" (shuffled), "Homie, play Careless Whisper", "Homie, play Hello by Adele", "Homie, play my running
playlist", then "pause", "play", "next song", "previous song", "louder", "quieter". Norwegian works too ("spill
de likte sangene mine", "neste sang", "skru ned musikken"). Music Assistant holds the Spotify sign-in and does
the search; the assistant only calls Home Assistant services (`music_assistant.search`, `get_library`,
`play_media`, and `media_player.*` on the Music Assistant player). `speaker:` in the settings file (or `SPEAKER=...`) is the
Music Assistant player's name or entity id. Needs Spotify Premium.

One-time setup, in Home Assistant: install the Music Assistant add-on and integration (done on 2026-09-24), open
Music Assistant from the sidebar, finish onboarding, and add the Spotify provider (sign in) and the Sonos
provider. If your Spotify playlists do not show up in Music Assistant's library, use "Synchronise now" in the
Spotify provider's menu. The engine logs "music assistant connected" with the player it will use. Try a request
without the microphone: `set -a; . ./.env; set +a; cargo run -p voice-assistant --example music -- "play my liked
songs"`.

Norwegian replies: Piper over the Wyoming protocol when `piper: host:port` is set (else macOS
`say`, Nora). `make piper` runs `rhasspy/wyoming-piper` in Docker on 127.0.0.1:10200; Home
Assistant's Piper add-on works too once its port is exposed, and in Kubernetes it can be a sidecar.
Voices: `piper-voice-no` (`no_NO-talesyntese-medium`, or `no_NO-nvcc-medium`); `english-tts: piper`
uses Piper for English as well (lighter than Pocket TTS on a weak CPU). Piper's 22.05 kHz output is
resampled to 24 kHz. `cargo run -p voice-assistant --example speak -- 127.0.0.1:10200
no_NO-nvcc-medium out.wav "Hei"` writes a sample.

Norwegian is off by default (`NORWEGIAN=true` turns it on). Off, everything is English: OpenAI's
`openai/whisper-base` transcribes (more accurate on English than NB-Whisper, and no language-ID model is
loaded). On, the listener below decides the language per utterance and NB-Whisper transcribes.

How it listens (default `LISTENER=whisper`, with `NORWEGIAN=true`):

- An energy detector cuts the microphone stream into utterances (speech ends after 640 ms of quiet).
- `openai/whisper-tiny` identifies the language. It is sure about English (above 0.9 on clear speech, with almost
  nothing on the Nordic languages) but spreads Norwegian over Danish, Swedish, English and more, so an utterance
  counts as English only when English scores at least 0.5 and at least 30 times Norwegian, Nynorsk, Danish and
  Swedish together; otherwise Norwegian. Measured: English 320-1500x, Norwegian at most 22x.
- `NbAiLab/nb-whisper-base` (National Library of Norway) transcribes in that language. Its own language guess
  leans Norwegian and then translates English speech, which is why it does not decide.
- The utterance counts when the name comes first (after an optional greeting, or at most two stray words), in
  any known spelling (`dialog::SPELLINGS`: Homie, Homey, ...). Whisper gets "<Name> is my voice assistant." as
  previous-text context, which teaches it the spelling; NB-Whisper gets "<Name>," instead.
- An utterance ends after 640 ms below the noise floor + 8 dB or 20 dB below its loudest part, whichever is
  higher, so quieter background talk does not keep it open. At most 10 s.

English speech: Kyutai Pocket TTS (100M parameters) through `ptts`, the Rust port on Laurent Mazare's `xn`
(pure Rust, no MKL or cmake). Weights and the 8 preset voices come from the ungated
`kyutai/pocket-tts-without-voice-cloning` repository (the main `kyutai/pocket-tts` repository is gated). Each
80 ms of audio is pushed to the speaker as soon as it is generated. On the M4 Pro CPU it runs 7x faster than
real time on 3 threads (f32) and 10x with `q8`, first audio 25-45 ms after the text is ready; even one thread is
3.6x. `voice-bench --bench pocket --precision q8 --threads 3` measures it on another host.

Language ID plus transcription takes about 0.2-0.3 s per utterance on the M4 Pro GPU, and the first reply audio
starts about 1 s after you stop talking in English: 0.64 s end-of-speech wait, about 0.3 s transcription, the
weather lookup (0.1-0.4 s), and a few tens of milliseconds for the first Pocket TTS audio.

`LISTENER=kyutai` uses Kyutai STT-1B instead: streaming, English only (it hears nothing useful in Norwegian),
lower latency, 2 GB of weights. It runs at about 20 ms per 80 ms frame on the M4 Pro GPU and 89 ms on its CPU,
already slower than real time. The homelab CPUs are slower than this Mac, so none of this is sized for the
cluster yet: listening needs to run on the CPU there (Whisper base on CPU, or quantized weights), while Pocket TTS
already fits a CPU.

`--processor passthrough` echoes PCM without loading a model. The Mimi weights (about 385 MB) are downloaded into
the Hugging Face cache on first use (`HF_HOME` moves it); `--mimi-model` takes a local file instead. Every server
flag has a `VOICE_*` environment variable, see `voice-engine --help`.

`--marker` reports the delay a listener would hear, without the audio devices: the 80 ms frame fill plus server
and network time for the slowest packet (gapless playback has to wait for that one), plus any shift the codec adds
to the content. On an M4 Pro with 4 threads the Mimi loop measures about 115 ms at p99 and 140 ms for the worst
packet; passthrough is about 87 ms.

Live mode (`--seconds`) prints mouth-to-ear with the audio devices included, from cpal's capture and playback
timestamps: the moment the microphone captured a packet to the moment the speaker plays it back. On an M4 Pro with
the built-in microphone and speakers (driver latency about 12 ms in and 13 ms out) that is 129 ms for passthrough
and 161-163 ms for Mimi at 4 threads. The client has no playout buffer, so once playback starts the delay stays
fixed until the speaker runs dry; each gap adds its length to everything after it, and the client reports it.

Make a test file on macOS:

```
say -o in.aiff "some words" && afconvert -f WAVE -d LEI16@24000 -c 1 in.aiff in.wav
```

## Smarter answers: name matching and a language model

Names are matched by sound against the home and the music library, so "the round lamb" switches
the Round lamp, "kakma de faka" plays Kakkmaddafakka and "my cost playlist" plays Kos. No setting.

With `llm-url` set, anything the rules do not understand goes to a language model on an
OpenAI-compatible API (Ollama, llama.cpp, vLLM or a hosted one), with the skills as tools: "it's too
dark in the kitchen" switches the lights, "and tomorrow?" after a weather answer gives tomorrow's,
general questions get a one-sentence answer, and speech meant for someone else is ignored (which is
also what lets follow-ups work without the name). The last four exchanges are its memory. Rules stay
first, so common requests do not wait for the model. `make ollama` runs Qwen3 4B instruct locally;
numbers in `docs/benchmarks.md`.

## Voice satellites (ESPHome): replacing Home Assistant's Assist

The engine can be the voice assistant of an ESPHome voice device (Home Assistant Voice PE, other
ESPHome voice builds, Linux Voice Assistant) instead of Home Assistant's Assist pipeline. It makes
the same native API connection Home Assistant makes (plaintext or Noise-encrypted, crate
`voice-esphome`), takes the microphone, and answers with its own listener, skills and voice.

1. Add the device to Home Assistant as usual (ESPHome integration), then disable the device's
   **Assist satellite** entity. A device streams to one voice assistant only; with that entity
   disabled, Home Assistant keeps the device's other controls (LED ring, volume, mute) and leaves
   the voice to the engine.
2. For the engine's own wake word (the name, "Homie"), set the device's wake word processing to
   **in Home Assistant**: the device then streams continuously and the engine listens for its name.
   With an on-device wake word ("Okay Nabu") the device starts a run itself and the next utterance
   is the request.
3. Settings: `satellite: <device ip>:6053`, and `satellite-key` (the device's base64 API encryption
   key, a secret: `VOICE_SATELLITE_KEY` or `VOICE_SATELLITE_KEY_FILE`). The device fetches spoken
   answers from the engine's HTTP port (`/speech/<id>.wav`); set `public-url` when the address it
   reaches the engine on is not the engine's own (Kubernetes LoadBalancer).

Answers start playing while they are still being synthesized, the way Home Assistant does it: the
answer's URL goes out with the run's start, the device is told to play it (`tts_start_streaming`)
with the first audio, and the HTTP server streams the wav (chunked) as it grows.
`satellite-stream: false` serves each answer once it is complete instead. An answer that ends in a
question ("Which room?", "For how long?") keeps the conversation open: the device listens again
without its wake word once it has spoken (`continue_conversation`).

Answers on another speaker: `answer-player: media_player.living_room` announces each answer on that
Home Assistant media player (a Sonos) instead of the device, over whatever it is playing, and
`answer-volume: 0.65` sets the announcement's volume. The device only listens then. The player fetches
the answer from `public-url` too, so it needs to reach the engine's HTTP port. An answer that ends in a
question does not keep the conversation open with a player, since the device would hear the question.

Wake words on the device: `satellite-wake-words: [Okay Nabu, Hey Jarvis]` turns on those of the device's own
wake words each time the engine connects. Home Assistant's wake word selects go through its Assist satellite
entity, which is disabled while the engine holds the device, so they no longer reach it.

The room: "turn off the lights" (or "in here") without a room means the satellite's room, its area in
Home Assistant, looked up by the device's name when it connects. The `room` setting overrides it,
and gives the Mac microphone a room too. "Turn them off" still means the lights switched last.

Under the hood the bridge opens an ordinary session on the engine's UDP port, so the engine serves
one satellite at a time, and `make assistant` cannot connect while a satellite is attached.
`cargo run -p voice-esphome --example fake_satellite -- 127.0.0.1:16053 question-16k.wav answer.wav`
pretends to be a device, for testing without hardware (`--speaker` for streamed answers, `--timers` for a
timer display, `--stay 15` to stay connected after the answer and see timers end, `--then reply.wav` for
the reply when the conversation stays open).

A page with everything the assistant understands: `web: true` serves it on the metrics port (`/`). Each
skill shows what it does, where its data comes from, the Home Assistant services or APIs it calls and phrases
that reach it; a box shows how a typed phrase is understood (`/api/parse?q=...`), without acting on it.

The JARVIS style: `honorific: sir` makes English answers sound like a butler ("As you wish, sir. The living
room lights are off."), with varied openers, a greeting after a quiet spell and now and then a remark that
fits the answer. With `english-tts: piper` and `piper-voice-en: jarvis-high`, the voice is the community
Piper model [jgkawell/jarvis](https://huggingface.co/jgkawell/jarvis): put `jarvis-high.onnx` and
`jarvis-high.onnx.json` in Piper's data directory (`~/Library/Caches/innestemme/piper` for `make piper`).

A more natural English voice: `english-tts: kokoro` speaks through [Kokoro](https://huggingface.co/hexgrad/Kokoro-82M)
behind Kokoro-FastAPI (`make kokoro` runs it on 127.0.0.1:8880; `kokoro-url` points elsewhere). `kokoro-voice` picks
the voice, default `am_onyx` (American, deep); `bm_george` is British. Audio streams as it is synthesized.

Pre-rendered speech: `speech-cache: <dir>` keeps every fixed English sentence once it is spoken (one folder per
voice): the persona's openers and remarks, lead-ins, all jokes (the built-in list, then) and confirmations such as
"The kitchen lights are off." The fixed ones are rendered ahead in idle time after start. Sentences with numbers are
live data and are always synthesized; a slow answer (weather, departures, prices, what's on, who's home) opens
with a lead-in ("Checking the forecast, sir.") that plays while its data sentence is synthesized.

On the web page, "Hear response" answers a typed request aloud in the browser, with the assistant's voice and
style, when answering changes nothing (weather, departures, prices, what's on, who's home, the shopping list);
requests that switch or play something are not run from the page.

Running a deployed device's engine from the Mac while working on it: stop the deployed engine (a device takes
one engine), put the deployed settings in `config.satellite.yaml` (git-ignored, same keys as above, plus
`metrics-bind: 0.0.0.0:9090` so the device or player reaches the Mac and `dump-utterances` to keep each
run as a wav), and run `make satellite`. The key is read from `VOICE_SATELLITE_KEY`, or from
`~/.config/innestemme/satellite.key` when that exists. The log is in `target/satellite.log`.

Timers on a device: devices with a timer display (the Voice PE's LED ring) get the ESPHome timer events
(started, updated, cancelled, finished) and count down and ring themselves, like with Home Assistant.
Reminders, and timers on devices without a display, are announced instead: the device fetches the chime and
the spoken text as a wav and plays it (`VoiceAssistantAnnounceRequest`).

## Configuration

Every option of `voice-engine --help` can be set three ways, with the same name: a flag
(`--wake-name Jarvis`), an environment variable (`VOICE_WAKE_NAME=Jarvis`), or a key in a YAML
settings file (`wake-name: Jarvis`) named by `--config` or `VOICE_CONFIG`. Flags win over the
environment, the environment over the file, the file over the built-in defaults.
`config.example.yaml` lists the personal ones; `make assistant` reads `config.local.yaml`
(git-ignored) when it exists. Unknown keys stop the engine with an error; at start it logs which
settings came from where, never their values.

- **Kubernetes:** mount the file from a ConfigMap and set `VOICE_CONFIG` to it. Secrets stay out of
  it: for any variable `X`, `X_FILE` reads the value from a file, so `HA_TOKEN_FILE` can point at a
  mounted Secret (or set `HA_TOKEN` from one).
- **Home Assistant add-on:** add-on options arrive as `/data/options.json`, which is valid YAML, so
  `VOICE_CONFIG=/data/options.json` works as is. With no Home Assistant configured and
  `SUPERVISOR_TOKEN` present, the engine uses the Supervisor's proxy (`http://supervisor/core`)
  and that token.

## Wire protocol

UDP, one datagram per 20 ms packet of 24 kHz mono s16 PCM (480 samples, 384 kbit/s). Four packets make one 80 ms
Mimi frame. `Codec::Opus` is reserved in the header but not implemented.

1. The client sends `Hello` with a session id and resends it until the server answers with `Hello`.
2. Audio `seq` starts at 0 after `Hello`. The server reorders within `--jitter-depth` packets (default 2) and
   conceals what is lost.
3. `Bye` closes the input. The server pads the last frame and still sends the tail of the output.

One session at a time. A client that disappears without `Bye` holds the session until `--session-timeout-secs`
(default 5) passes; `Hello` from another address is refused until then and counted in
`voice_packets_rejected_total`. The test client keeps retrying for 7 s to cover this.

## Metrics

`/metrics` is Prometheus text, `/healthz` answers 200. Histograms: `voice_jitter_wait_seconds`,
`voice_mimi_encode_seconds`, `voice_engine_step_seconds`, `voice_mimi_decode_seconds`,
`voice_frame_process_seconds`, `voice_frame_total_seconds`. The gauge `voice_realtime_factor` must stay above 1.
Counters worth alerting on: `voice_ring_overruns_total`, `voice_process_errors_total`,
`voice_packets_lost_total`.

## Development

```
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`crates/voice-engine/tests/loopback.rs` streams through a real server with 5% loss, reordering and a duplicate, and
runs under an allocator that aborts on any allocation in a guarded section.

The toolchain is pinned in `rust-toolchain.toml` so a new clippy release cannot break the build unannounced.

Platform notes:

- x86-64 Linux builds use `-C target-cpu=x86-64-v3` (set in `.cargo/config.toml`). candle picks its SIMD kernels at
  compile time, so a baseline build silently runs scalar code. The server logs `avx=true` at startup when it is
  right.
- aarch64 Linux (for example Docker on Apple Silicon) needs
  `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+fp16"`, otherwise `gemm-f16` fails to
  assemble.
- `voice-client` needs ALSA headers on Linux (`libasound2-dev`, `pkg-config`). The server does not.

## Container

```
docker build -t innestemme .
docker run --rm -p 7000:7000/udp -p 9090:9090 -v voice-models:/models innestemme
docker pull ghcr.io/mortennordbye/innestemme:latest    # built by CI from main
```

The image runs as uid 65532 with `HF_HOME=/models` and works with a read-only root filesystem and all
capabilities dropped. It is linux/amd64 only and requires an x86-64-v3 CPU (AVX2). The build stage cross-compiles
when Docker runs on another architecture, so it does not go through emulation.

---

## Workflows

| Workflow | Trigger | Purpose |
|---|---|---|
| CI | push, PR | clippy and tests; on main, build the image, smoke-test it, scan it with Trivy and push it to GHCR |
| Dependency Review | PR | block dependency changes with known vulnerabilities |
| Scorecard | push, weekly | OpenSSF supply-chain score |
| GHCR retention | weekly | keep the newest image versions, delete the rest |

---

<div align="center">

### ⭐ Star this repo if you find it useful ⭐

<a href="https://www.star-history.com/#mortennordbye/innestemme&Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=mortennordbye/innestemme&type=Date&theme=dark" />
    <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=mortennordbye/innestemme&type=Date" />
    <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=mortennordbye/innestemme&type=Date" width="600" />
  </picture>
</a>

Made by [Morten Victor Nordbye](https://github.com/mortennordbye)

</div>
