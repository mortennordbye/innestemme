# How it works

One process, `voice-engine --processor assistant`, holds a voice satellite (or the Mac's microphone through
`voice-client`), listens, decides and speaks. Nothing leaves the house except the lookups a skill makes (Yr,
Entur, Wikipedia, news feeds, electricity prices).

## A request, step by step

As deployed: a Home Assistant Voice PE in the living room, answers on a Sonos.

| Step | Where | Time |
|---|---|---|
| 1. The Voice PE streams its microphone all the time (16 kHz) | `firmware/`, `voice-esphome` | |
| 2. openWakeWord scores the stream; "Hey Jarvis" above `wake-threshold` starts a request | `voice-assistant/src/wakeword.rs` | about 1 ms per 80 ms of audio |
| 3. Music that plays in the room is paused (queued, see below) | `assistant.rs` `hush` | |
| 4. An energy detector cuts the request: a draft after 0.24 s of quiet, the end after 0.64 s | `voice-assistant/src/vad.rs` | 0.24 or 0.64 s |
| 5. Parakeet TDT 0.6B (int8, sherpa-onnx) transcribes the draft while the pause goes on | `parakeet.rs` | about 70 ms on an M4 Pro |
| 6. The rules parse it into an intent; a complete command acts on the draft at once | `intent.rs`, `assistant.rs` `complete_at_once` | under 1 ms |
| 7. The skill runs and words the answer; Home Assistant writes go to a queue | `assistant.rs`, `ha.rs`, skill modules | 0-50 ms for most |
| 8. The persona styles it ("Very good, sir.") | `persona.rs` | |
| 9. Each sentence comes from the speech cache, or from Kokoro | `speech_cache.rs`, `kokoro.rs` | 0 ms cached, 1-3 s on the cluster otherwise |
| 10. Home Assistant announces the answer's URL on the Sonos, which streams it as it grows | `satellite.rs` | about 0.4 s until it sounds |

So a cached answer is heard about 0.65 s after the end of a command that acts on its draft, and about 1.05 s
after one that waits out the full pause. `docs/benchmarks.md` has the measurements.

## The parts

**Listener.** `listener: parakeet` (English) in the cluster. `listener: whisper` (the default) is the only one that
understands Norwegian: `openai/whisper-tiny` picks the language, `NbAiLab/nb-whisper-base` transcribes. Both are
called once per utterance the energy detector cuts. The wake word is either openWakeWord in the engine
(`wake-model`, on a satellite's continuous stream) or the name at the start of the transcript ("Homie, what's the
weather?"), with known misspellings (`dialog.rs`).

**Turns.** After an answer the assistant listens a few seconds more without the wake word (6 s on a satellite,
8 s on the Mac), for recognised requests only, so room chatter is ignored. Speech that repeats an answer of the last 30 s is the Sonos heard back and is dropped. An
unrecognised request after a wake gets "Sorry, I didn't catch that." only when Parakeet hears the name in the wake
word itself, so a false wake by the TV stays silent. That check runs as soon as the device wakes, while the
request is still being said, so the reply does not wait for it.

**Rules first.** `intent.rs` handles every skill in the catalogue in English and Norwegian. A request the rules do
not understand goes to a language model when `llm-url` is set (any OpenAI-compatible API, with the skills as
tools), else gets the short fallback. The cluster runs without one.

**Skills.** One module each under `voice-assistant/src/` (weather, transit, power, music, timers, shopping,
house, tv, news, lookup, calc, distance, party, smalltalk, jokes). `catalogue.rs` lists them for people: what each
does, its data source, the calls it makes and example phrases, which the tests parse.

**Home Assistant.** REST only (`ha.rs`): one template call lists every light with its area, service calls do the
rest. Writes that the answer does not depend on (lights, scenes, the lockdown, routines, ringing a phone, pausing
and resuming music) go to a queue on a thread of their own and run in order, because Home Assistant answers a
Hue call only once the lights have switched (0.5-0.7 s). A failed queued call is said afterwards.

**Speech.** English through Kokoro, voice `am_onyx`: from a Kokoro-FastAPI server (`english-tts: kokoro`, a
Python sidecar in the cluster) or inside the engine through sherpa-onnx (`english-tts: kokoro-onnx`, the f32
v1.0 model, 16-31% faster on the same CPUs and no server). Norwegian through Piper over Wyoming or macOS `say`. `speech-cache` keeps every fixed sentence on disk, one folder per voice, and
renders them ahead while idle (about 5,500: openers, remarks, lead-ins, jokes and small talk in each way the persona puts them, confirmations,
every volume level, the time left on a single timer, every minute of the day, the home's lights and room
temperatures from Home Assistant). Sentences with numbers are
kept only for a few frequent shapes. Fixed wording and live data are separate sentences, so only the data is
synthesized. A slow skill opens with a cached lead-in ("Checking the forecast, sir.") while it fetches. The home's forecast
and the electricity prices are fetched every 4 minutes while idle, so those answers need no request and skip the
lead-in, which would only hold them back by its own 2 s. A
sentence heard before it was rendered is logged as `speech not rendered ahead`.

**Satellite bridge.** `voice-esphome` speaks the ESPHome native API (plaintext or Noise) the way Home Assistant
does, so the engine takes the device's microphone in place of Assist. Answers are wavs on the engine's HTTP port
(`/speech/<id>.wav`), streamed while they are synthesized; with `answer-player` Home Assistant announces them on
another media player. Timers count down on the Voice PE's LED ring through the ESPHome timer events.

**Screens.** `web: true` serves on the metrics port. `/` is the dashboard: Home (an overview: the face, the
clock and forecast, every room's lights and climate, buttons for the whole apartment (all on, all off, one colour
everywhere, party mode, the lockdown), each room's temperature, quick switches, the house's status, what plays, the
shopping list, the latest requests and today's numbers), Lights, Climate (trends over 24 hours, the forecast,
sensors, batteries), Media, House (people, the alarm, the camera, automations, the network, updates, every device's
settings), Map (every skill, and how a phrase is understood), Log (what no skill handled) and System (version,
models, connections, the speech cache, response times). `/tv` is the screen "show the weather on the TV" opens.
Rooms and devices come from Home Assistant (`control.rs`, one template call that all screens share); nothing is
listed in the code. The page has no login and the TV opens it too, so the engine allows it lights, scenes,
switches, players, selects, numbers, the shopping list and arming the alarm, and refuses disarming, buttons
(restarts), updates and the network's switches other than the guest Wi-Fi. `/api/activity` has the requests since
the start (in memory, about a day's worth).

**Real-time audio path.** The UDP transport, jitter buffer and model thread (`voice-proto`, `voice-rt`,
`voice-engine`) do no heap allocation after start; `tests/loopback.rs` checks that under an allocator that aborts.
The original codec loop (`--processor mimi`, Kyutai's Mimi) and `--processor passthrough` still run on the same
path, for measuring transport latency.

## Wire protocol

UDP, one datagram per 20 ms packet of 24 kHz mono s16 PCM (480 samples, 384 kbit/s), 12-byte header.

1. The client sends `Hello` with a session id and resends it until the server answers with `Hello`.
2. Audio `seq` starts at 0 after `Hello`. The server reorders within `--jitter-depth` packets (default 2) and
   conceals what is lost.
3. `Bye` closes the input. The server pads the last frame and still sends the tail of the output.

One session at a time; a satellite bridge opens an ordinary session, so `make assistant` cannot connect while a
satellite is attached. A client that disappears without `Bye` holds the session for `--session-timeout-secs`
(default 5). `Codec::Opus` is reserved in the header but not implemented.

## Metrics

`/metrics` is Prometheus text, `/healthz` answers 200. Histograms: `voice_jitter_wait_seconds`,
`voice_mimi_encode_seconds`, `voice_engine_step_seconds`, `voice_mimi_decode_seconds`,
`voice_frame_process_seconds`, `voice_frame_total_seconds`; the gauge `voice_realtime_factor` must stay above 1.
Counters worth alerting on: `voice_ring_overruns_total`, `voice_process_errors_total`, `voice_packets_lost_total`.
The assistant's own timings are log lines: `heard` (transcription), `answering ... after` (deciding),
`spoken ... after_speech` (end of speech to the first answer audio) and `answer heard on the device` (the Sonos
starting).
