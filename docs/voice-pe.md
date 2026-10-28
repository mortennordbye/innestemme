# Voice satellites (Home Assistant Voice PE)

The engine can be the voice assistant of an ESPHome voice device (Home Assistant Voice PE, other ESPHome voice
builds, Linux Voice Assistant) instead of Home Assistant's Assist pipeline. It makes the same native API
connection Home Assistant makes (plaintext or Noise-encrypted, crate `voice-esphome`), takes the microphone, and
answers with its own listener, skills and voice.

## Setting one up

1. Add the device to Home Assistant as usual (ESPHome integration), then disable its **Assist satellite**
   entity. A device streams to one voice assistant only; with that entity disabled, Home Assistant keeps the
   device's other controls (LED ring, volume, mute) and leaves the voice to the engine.
2. Flash the firmware in [`firmware/`](../firmware/README.md) (`DEVICE=<ip> make firmware-flash`) and leave its
   **Wake word in innestemme** switch on: the device then streams its microphone all the time and the engine
   detects the wake word. The official firmware has no such mode; with it, the device's own wake word starts a
   run and the next utterance is the request.
3. Settings: `satellite: <device ip>:6053`, and the device's API encryption key (base64, a secret) in
   `VOICE_SATELLITE_KEY` or `VOICE_SATELLITE_KEY_FILE`. The key is the `noise_psk` Home Assistant set when the
   device was added (the ESPHome entry in `.storage/core.config_entries`).
4. Wake word in the engine: `wake-model: hey_jarvis` (an openWakeWord model, downloaded on first start to
   `$HF_HOME/openwakeword`; the pretrained models are CC BY-NC-SA 4.0) or the path to a `.onnx` model of your
   own, and `wake-threshold` (0.65 in the cluster). Scores from 0.2 up to the threshold are logged as
   `wake word missed`. With `wake-check-from` (0.15 in the cluster), a miss that peaks at or above it goes to
   Parakeet, and the device wakes when the name is in it (`wake word confirmed by its name`, else `no name in
   the wake word`). There, a few everyday words Parakeet writes for the name also count right after "hey"
   ("Hey German", `AFTER_GREETING` in `dialog.rs`). The model scores some voices well under the threshold, and the TV hardly ever says the
   name. The check adds about 0.3 s to those wakes; the audio said meanwhile is kept for the request.
5. Answers: the device fetches them from the engine's HTTP port (`/speech/<id>.wav`); set `public-url` when the
   address it reaches the engine on is not the engine's own (a Kubernetes LoadBalancer). `answer-player:
   media_player.living_room` announces them on a Home Assistant media player (a Sonos) instead, at
   `answer-volume`; the device only listens then.

The engine serves one satellite at a time and reconnects with backoff when the connection drops. Only one engine
can hold a device: stop a laptop instance before the cluster's starts, or the two take turns.

## What it does with the device

- **Streaming answers.** The answer's URL goes out with the run's start, the device is told to play it
  (`tts_start_streaming`) with the first audio, and the wav is served chunked as it grows.
  `satellite-stream: false` serves each answer once it is complete.
- **Questions keep the conversation open.** An answer that ends in "?" ("Which room?", "For how long?") makes the
  device listen again without the wake word (`continue_conversation`); not with an answer player, which the
  device would hear.
- **Follow-ups.** With `wake-model` and an answer player, the engine listens for 6 s after an answer has played
  ("And turn off the kitchen."), recognised requests only.
- **Interrupting.** "Hey Jarvis" over an answer stops it.
- **The room.** "Turn off the lights" without a room means the satellite's room: its area in Home Assistant,
  looked up by the device's name when it connects. `room` overrides it. Renaming the device in Home Assistant
  breaks the lookup.
- **The LED ring is not a lamp.** Home Assistant lists the ring (and network gear's status LEDs) as lights;
  "all the lights", "everything off" and a room leave anything with "LED" in its name alone. Naming it
  ("turn off the LED ring") still works.
- **Timers.** The Voice PE's LED ring gets the ESPHome timer events and counts down and rings itself; it stops
  on "stop", the wake word or the button (15 minutes at most), and the engine cannot stop it, since the device
  does not stream while ringing. Reminders, and timers on devices without a display, are announced instead.
- **The device's own wake words.** `satellite-wake-words: [Hey Jarvis]` turns on those of the device's wake
  words each time the engine connects; Home Assistant's selects for them go through the disabled satellite
  entity and no longer reach it.
- **Recordings.** `dump-utterances: <dir>` keeps every utterance and run as a wav; a run's wav starts two seconds
  before the wake word. `examples/wake_score.rs` and `examples/wake_text.rs` score them.

## When it seems to stop responding

- **Check both sides before guessing.** The engine log shows what was heard and answered
  (`kubectl -n innestemme logs deploy/innestemme`); a working start logs `satellite connected` and
  `satellite room from home assistant`. The device's own log (any ESPHome API client calling `subscribe_logs`
  with its key) shows its detections and state changes.
- **A loud answer player next to the device deafens its own wake word detector** for several seconds; the
  device's echo cancellation only removes its own speaker. With the wake word detected in the engine this
  matters less; keep `answer-volume` moderate (0.50 in the cluster).
- **The device's own Hey Jarvis model is marginal:** a real voice scored 0.84 to 0.86 against its most sensitive
  cutoff of 0.83, where openWakeWord scored the same voice at 0.99 and ordinary speech at 0.24 at most. That is
  why `wake-model` exists.
- **Saying the wake word again during a run stops the run** on the official firmware (the engine logs
  `device ended the run`); the patched firmware ignores a second detection within 3 s.
- **A stuck device** can be restarted with its Restart button (shown by the patched firmware) or over the API.
- **Testing without talking:** `cargo run -p voice-esphome --example fake_satellite -- 127.0.0.1:16053
  question-16k.wav answer.wav` pretends to be a device (`--speaker` for streamed answers, `--timers` for a timer
  display, `--stay 15` to stay connected, `--then reply.wav` for a reply when the conversation stays open,
  several wavs comma separated for one question after another). Playing a synthetic "Hey Jarvis" on the real
  speaker works too, but is heard in the room.
