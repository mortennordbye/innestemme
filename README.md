# ai-voice

A local, CPU-only voice engine in Rust. Audio comes in over UDP, passes through a jitter buffer, is encoded to
Mimi codes, handed to an `Engine`, decoded back to audio and sent out again. The audio path does no heap
allocation after startup, and every stage reports its latency.

Status: Milestone 1 (core audio loop). The only engine so far is `LoopbackEngine`, which echoes the codes back, so
what you hear is your own voice after a Mimi round trip. No STT, LLM, TTS or Home Assistant integration yet.

## Layout

| crate | what it is |
|---|---|
| `voice-proto` | Wire format: 12-byte header, `Hello` / `Audio` / `Bye`, PCM helpers. |
| `voice-rt` | Real-time plumbing: reorder buffer, atomic histogram, allocation guard, pinned threads. |
| `voice-codec` | Packet codec (raw PCM) and `MimiCodec` over `moshi` / candle. |
| `voice-engine` | The server: UDP transport, net task and model thread joined by lock-free rings, `/metrics`. |
| `voice-client` | Test client: tone, wav file or live microphone. Reports frame turnaround. |
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

`--processor passthrough` echoes PCM without loading a model. The Mimi weights (about 385 MB) are downloaded into
the Hugging Face cache on first use (`HF_HOME` moves it); `--mimi-model` takes a local file instead. Every server
flag has a `VOICE_*` environment variable, see `voice-engine --help`.

`--marker` reports the delay a listener would hear, without the audio devices: the 80 ms frame fill plus server
and network time for the slowest packet (gapless playback has to wait for that one), plus any shift the codec adds
to the content. On an M4 Pro with 4 threads the Mimi loop measures about 115 ms at p99 and 140 ms for the worst
packet; passthrough is about 87 ms.

Make a test file on macOS:

```
say -o in.aiff "some words" && afconvert -f WAVE -d LEI16@24000 -c 1 in.aiff in.wav
```

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
docker build -t ai-voice .
docker run --rm -p 7000:7000/udp -p 9090:9090 -v voice-models:/models ai-voice
```

The image runs as uid 65532 with `HF_HOME=/models` and works with a read-only root filesystem and all
capabilities dropped. It is linux/amd64 only and requires an x86-64-v3 CPU (AVX2). The build stage cross-compiles
when Docker runs on another architecture, so it does not go through emulation.
