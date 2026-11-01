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
automation that loops cannot talk over everything.

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
