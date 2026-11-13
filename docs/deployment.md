# Deployment

## The image

CI builds `ghcr.io/mortennordbye/innestemme` on every push to `main`, tagged `0.0.<commit time>`, `sha-<commit>` and `latest`,
after clippy and the tests: it smoke-tests `/healthz`, scans the image with Trivy and pushes it.

```
docker build -t innestemme .
docker run --rm -p 7000:7000/udp -p 9090:9090 -v voice-models:/models innestemme
```

The image runs as uid 65532 with `HF_HOME=/models`, works with a read-only root filesystem and all capabilities
dropped, and is linux/amd64 only with an x86-64-v3 CPU (AVX2): candle picks its SIMD kernels at compile
time, and the server logs `avx=true` at start when that is right. The build stage cross-compiles when
Docker runs on another architecture.

## In the homelab cluster

The manifests are in the homelab repo, `k8s/talos/apps/innestemme/`, deployed by Argo CD; its
`docs/apps/innestemme/README.md` has the cluster's specifics (addresses, the device, firewall rules).

| | |
|---|---|
| Delivery | Kargo project `innestemme-cd`: each new image opens a promotion pull request in the homelab repo; merging it deploys. One open promotion blocks newer ones. |
| Settings | ConfigMap `innestemme-config`, mounted as `/config/config.yaml` (`VOICE_CONFIG`). Reloader restarts the pod when it or the Secret changes. |
| Secrets | `innestemme-secret` from Bitwarden Secrets Manager through an ExternalSecret: `HA_TOKEN`, `VOICE_SATELLITE_KEY`, `VOICE_ADDRESS`, `VOICE_HUE_APP_KEY`, `VOICE_HUE_CLIENT_KEY`, `VOICE_PLEX_TOKEN`. |
| Voice | Kokoro-FastAPI (`ghcr.io/remsky/kokoro-fastapi-cpu`) as a native sidecar on `127.0.0.1:8880`, so the engine starts once it answers; `OMP_NUM_THREADS` matches its CPU limit. |
| Volume | `models-pvc` on `/models`: the Parakeet model (630 MB) and openWakeWord under `hub/` and `openwakeword/`, the speaker model (30 MB) under `speaker/`, the speech cache (about 560 MB and growing a little), `unhandled.jsonl`, and next to it `learned.json`, `timers.json` (running timers, reminders and alarms, so a deploy keeps them) and `memory.json` (the notes). Everything on it downloads or renders again. |
| Network | A LoadBalancer on port 9090: spoken answers for the Sonos, `/metrics`, the web pages. |

Settings worth knowing in the ConfigMap: `listener: parakeet`, `english-tts: kokoro` with `kokoro-voice:
am_onyx`, `speech-cache: /models/speech-cache`, `honorific: sir` and `honorific-female: ma'am` (the speaker model under `speaker/`), `wake-model: hey_jarvis` with
`wake-threshold: 0.65` and `wake-check-from: 0.15`, `answer-player` and `answer-volume`, `hue-bridge` for party mode, `plex-url` (the Plex service in the cluster; its CiliumNetworkPolicy lets the innestemme namespace in), `power-cycle`,
`unhandled-log`. No `llm-url`: rules only.

## Working on it against the real device

The device takes one engine at a time. Scale the cluster's to zero (`kubectl -n innestemme scale
deploy/innestemme --replicas=0`; Argo CD ignores replicas), then `make kokoro` and `make satellite` with the
cluster's settings in `config.satellite.yaml` (git-ignored; add `metrics-bind: 0.0.0.0:9090` so the Sonos
reaches the Mac, and `dump-utterances` to keep each run). Scale back to one afterwards.

## Configuration

Every option of `voice-engine --help` can be set three ways, with the same name: a flag (`--wake-name Jarvis`),
an environment variable (`VOICE_WAKE_NAME=Jarvis`), or a key in a YAML file (`wake-name: Jarvis`) named by
`--config` or `VOICE_CONFIG`. Flags win over the environment, the environment over the file, the file over the
defaults; a few take a plain variable as well (`HA_URL`, `HA_TOKEN`), which also beats the file. Unknown keys
stop the engine; at start it logs where each setting came from, never its value. `config.example.yaml` describes
the personal ones.

- **Secrets:** for any variable `X`, `X_FILE` reads the value from a file, so `HA_TOKEN_FILE` can point at a
  mounted Secret.
- **Home Assistant add-on:** add-on options arrive as `/data/options.json`, which is valid YAML, so
  `VOICE_CONFIG=/data/options.json` works as is. With no Home Assistant configured and `SUPERVISOR_TOKEN`
  present, the engine uses the Supervisor's proxy (`http://supervisor/core`) and that token.

## Announcements from Home Assistant

With `web: true` and an `announce-token` (`VOICE_ANNOUNCE_TOKEN`, or `VOICE_ANNOUNCE_TOKEN_FILE` for a mounted
Secret), Home Assistant automations can have the assistant speak: the doorbell, the washing machine being done.
Without a token the endpoint does not exist. `POST /api/announce` on the engine's HTTP port takes the text in the
query and the token as a bearer header:

| Query | |
|---|---|
| `text` | What to say, up to 300 characters. |
| `room` | Optional: a room as people say it (`kitchen`, `stua`). Its speaker is a media player in that Home Assistant area that can announce, the speaker's own before Music Assistant's. A room without one falls back to where answers play. |
| `player` | Optional: a media player entity id, which wins over `room`. |
| `lang` | `en` (default) or `no`. |

It answers `202` once the text is queued: it is spoken after whatever the assistant is saying, where answers play
(`answer-player`, else the satellite), as an announcement over the music. `room` and `player` need
`answer-player`, whose Home Assistant plays them. More than 5 announcements in a minute get `429`, so an
automation that loops cannot talk over everything. A request without the right token gets `401` before anything
else is checked, and after 10 of those in a minute from one address that address gets `429` until the minute has
passed. A token shorter than 16 characters leaves announcements off, with a warning in the log. The text is
synthesized fresh each time and never kept in the speech cache.

The answer player is reached through the satellite's session: with the Voice PE disconnected (a restart, the
device off), announcements go to the engine's own audio output only and are not heard in the house.

In Home Assistant, a `rest_command` in `configuration.yaml`, with the token in `secrets.yaml`:

```yaml
rest_command:
  innestemme_announce:
    url: "http://<engine host>:9090/api/announce?text={{ text | urlencode }}&room={{ room | default('') | urlencode }}"
    method: post
    headers:
      authorization: !secret innestemme_announce_bearer   # "Bearer <announce-token>"
```

and in an automation:

```yaml
actions:
  - action: rest_command.innestemme_announce
    data:
      text: "Someone is at the door."
      room: kitchen
```

## Requests from a phone

With `web: true` and an `ask-token` (`VOICE_ASK_TOKEN`, or `VOICE_ASK_TOKEN_FILE` for a mounted Secret), a phone
can ask the assistant anything it is asked by voice: lights, music, the weather, a timer. Without a token the
endpoint does not exist. Give it a token of its own, not the `announce-token`, so either can be revoked alone.
`POST /api/ask` on the engine's HTTP port takes the request in the query and the token as a bearer header:

| Query | |
|---|---|
| `q` | The request, as it would be said, up to 300 characters. A leading name ("Jarvis, ...") is dropped. |

It runs the request through the same rules, skills and language model as a spoken one and answers `200` with
`{"answer": "..."}` once it is done, for the phone to speak or show, in the persona's style as it would be
spoken. The answer is not spoken in the house, but
what the request does is done as if it was said to the satellite: "turn off the lights" without a room means the
satellite's room (or `room`), and "play jazz" plays on its speaker. A request that ends quietly ("never mind")
answers with an empty `answer`. More than 10 requests in a minute get `429`.
Without the right token it answers `401` before anything else is checked, with the same lockout as
announcements: 10 wrong tokens in a minute from one address get `429` until the minute has passed. A token
shorter than 16 characters leaves it off, with a warning in the log.

The endpoint can do whatever a voice in the room can, so serve it only where the house is: the cluster's private
gateway, reached away from home over the VPN. Behind a proxy every caller has the proxy's address, so wrong
tokens lock out the proxy for that minute.

For every user of a Home Assistant at once, the `innestemme` integration makes it Assist's conversation agent
([home-assistant.md](home-assistant.md)). For one phone without Home Assistant, an iPhone shortcut named after the
assistant ("Jarvis") makes "Hey Siri, Jarvis" ask it, from the lock screen, the Action Button or a widget too. In
the Shortcuts app:

1. Dictate Text (language English, stop listening after a pause).
2. URL Encode the dictated text.
3. Get Contents of URL: `https://<engine host>/api/ask?q=` followed by the encoded text, method `POST`,
   header `Authorization` with the value `Bearer <ask-token>`.
4. Get Dictionary Value, key `answer`, from the contents.
5. Speak Text, with that value.

From a shell, with the token in `ASK_TOKEN`:

```
curl -X POST -H "Authorization: Bearer $ASK_TOKEN" \
  "https://<engine host>/api/ask?q=what%27s+the+weather+tomorrow"
```
