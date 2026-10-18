<div align="center">

# 🤫 innestemme

### A local voice assistant that replaces Home Assistant's Assist, on your own CPU.

[![Rust](https://img.shields.io/badge/Rust-000000?logo=rust&logoColor=white)](https://www.rust-lang.org) [![Home Assistant](https://img.shields.io/badge/Home%20Assistant-18BCF2?logo=homeassistant&logoColor=white)](https://www.home-assistant.io) [![ESPHome](https://img.shields.io/badge/ESPHome-000000?logo=esphome&logoColor=white)](https://esphome.io) [![Ollama](https://img.shields.io/badge/Ollama-000000?logo=ollama&logoColor=white)](https://ollama.com)

[![CI](https://github.com/mortennordbye/innestemme/actions/workflows/ci.yml/badge.svg)](https://github.com/mortennordbye/innestemme/actions/workflows/ci.yml) [![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/mortennordbye/innestemme/badge)](https://securityscorecards.dev/viewer/?uri=github.com/mortennordbye/innestemme)

[![License](https://img.shields.io/github/license/mortennordbye/innestemme?style=flat-square)](LICENSE) [![Last Commit](https://img.shields.io/github/last-commit/mortennordbye/innestemme?style=flat-square)](https://github.com/mortennordbye/innestemme/commits/main) [![Stars](https://img.shields.io/github/stars/mortennordbye/innestemme?style=flat-square)](https://github.com/mortennordbye/innestemme/stargazers)

*Innestemme* is Norwegian for "indoor voice": a voice assistant that never leaves the house. Wake word, speech
recognition, the rules and the voice all run locally on a CPU, it drives Home Assistant, and it answers through
ESPHome satellites such as the Home Assistant Voice PE.

</div>

---

## Overview

A Voice PE streams its microphone to the engine, which hears "Hey Jarvis" (openWakeWord), transcribes the request
with Parakeet, answers from its own rules and skills, and speaks with Kokoro, here on the living room Sonos.
Common answers are rendered ahead, so a light switched by voice is confirmed about 0.65 s after you stop talking.
A local language model (any OpenAI-compatible server) can take what the rules do not understand. English is the
focus; Norwegian works with Whisper and Piper.

| Skill | Examples |
|---|---|
| Lights | "turn off the kitchen lights", "dim the living room to 30 percent", "make the bedroom red", "which lights are on?", "undo" |
| Scenes and scripts | "set the living room to relax", "run movie time" |
| Music | "play my liked songs", "play Hello by Adele", "next song", "volume 40 percent" (Music Assistant) |
| Timers and reminders | "set a pasta timer for 8 minutes", "remind me in 20 minutes to check the oven" |
| TV and its screen | "turn off the TV", "put Netflix on the TV", "show the weather on the TV" |
| Routines | "I'm leaving", "I'm home", "good night" |
| Just for fun | "initiate lockdown" (red lights, Hans Zimmer) and "stand down", "party mode" (every Hue light follows the music) |
| House | "is everything okay at home?", "who's home?", "how warm is it in the bedroom?", "is the alarm armed?" |
| Phones | "where's my phone?" (rings it, even on silent), "how much battery does my phone have?" |
| Guest Wi-Fi, camera, alarm | "turn on the guest Wi-Fi", "turn on privacy mode", "arm the alarm" (never disarmed by voice) |
| Weather, departures, power | "will it rain in Bergen tomorrow?", "when's the next bus?", "when is electricity cheapest?" (Yr, Entur, hvakosterstrommen.no) |
| Everyday | "good morning" (a briefing), "what's the latest news?", "who is Jonas Gahr Støre?", "15 percent of 80", "how far is Tokyo?", "tell me a joke", "what time is it?" |
| Shopping list | "add milk and eggs to the shopping list", "what's on the list?" |
| Small talk | "do you love me?", "you're useless" (it has comebacks), "will you marry me?", "what's the meaning of life?", "open the pod bay doors"; "shut up" just stops it |
| Safety alerts | spoken unasked when a smoke, carbon monoxide, gas, water or breaking glass sensor goes off |

The engine's web page lists every skill with its data source, the calls it makes and phrases that reach it.

## Quick start

On a Mac, with the microphone:

```
make setup                      # release build with Metal, downloads the models
cp config.example.yaml config.local.yaml   # name, home, speaker; HA_URL and HA_TOKEN go in .env
make kokoro                     # the English voice, in Docker
make assistant                  # say the name, then the request: "Homie, what's the weather?"
```

For a Voice PE, see [docs/voice-pe.md](docs/voice-pe.md); for the cluster, [docs/deployment.md](docs/deployment.md).

## Documentation

| | |
|---|---|
| [docs/architecture.md](docs/architecture.md) | how a request flows and what each step costs, the parts, the wire protocol, metrics |
| [docs/voice-pe.md](docs/voice-pe.md) | the Voice PE as a satellite: setup, what the engine does with it, troubleshooting |
| [firmware/README.md](firmware/README.md) | the patched Voice PE firmware (wake word in the engine) |
| [docs/deployment.md](docs/deployment.md) | the image, the homelab cluster, configuration and secrets |
| [docs/development.md](docs/development.md) | tests, the Mac loop, the examples and the latency bench |
| [docs/benchmarks.md](docs/benchmarks.md) | every measurement so far: listeners, voices, end to end, latency |
| [config.example.yaml](config.example.yaml) | the personal settings, explained |
| [AGENTS.md](AGENTS.md) | conventions for changes |

## Layout

| crate | what it is |
|---|---|
| `voice-proto` | Wire format: 12-byte header, `Hello` / `Audio` / `Bye`, PCM helpers. |
| `voice-rt` | Real-time plumbing: reorder buffer, atomic histogram, allocation guard, pinned threads. |
| `voice-codec` | Packet codec (raw PCM) and `MimiCodec` over `moshi` / candle. |
| `voice-engine` | The server: UDP transport, model thread, assistant worker, ESPHome satellite bridge, party mode, web pages, settings, `/metrics`. |
| `voice-assistant` | Listeners (Parakeet, Whisper), wake word and turns, intent rules, the language model client, the skills, Home Assistant and Music Assistant, speech output (Kokoro, Piper, `say`) and the speech cache. |
| `voice-esphome` | ESPHome native API client (plaintext and Noise). |
| `voice-client` | Test client: tone, wav file or live microphone; reports frame turnaround and mouth-to-ear. |
| `voice-bench` | Per-frame step times for [docs/benchmarks.md](docs/benchmarks.md). |

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
