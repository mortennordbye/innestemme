# Voice PE firmware

**Not ready to flash.** Measured on 2026-10-10 with the same test clip from the same spot, this build scored
0.70 to 0.86 on "Hey Jarvis" where the official 26.9.0 binary scored 0.88 to 0.96, so real voices were missed.
The likely cause is `voice_kit`, which upstream's YAML takes from the `dev` branch rather than the release;
pin it to the release and compare scores before flashing again. The device runs the official binary.

The official [Home Assistant Voice PE firmware](https://github.com/esphome/home-assistant-voice-pe) at a pinned
release, with `voice-pe.patch` on top. `make firmware` builds it with ESPHome in Docker; `DEVICE=<ip> make
firmware-flash` builds and flashes it over Wi-Fi. Wi-Fi and the API encryption key stay as the device has them.
If a flash goes wrong, the device can be flashed over USB-C from https://web.esphome.io.

What the patch changes:

- A second wake word detection within 3 s of the last start is ignored. The official firmware treats any
  detection during a run as "stop", so one "Hey Jarvis" detected twice ended the request being spoken.
- A "Hey Jarvis threshold" number (0.50 to 0.99): the detection cutoff in small steps, instead of the three
  sensitivity presets (Very sensitive is 0.83). Lower wakes more easily and falsely more often.
- The Restart button is shown, so a stuck device can be restarted from Home Assistant.
- No official firmware update entity, so Home Assistant does not offer to replace this build.

To move to a newer release: set `FIRMWARE_REF` (and `ESPHOME_IMAGE` to its `min_version`), and refresh the
patch if it no longer applies.
