# Benchmarks (M0)

Per-frame step time of the speech stack. One frame is 80 ms of audio, so a stage keeps up with real time when its
`whole frame` mean stays under 80 ms (RTF above 1). RTF is 80 ms divided by the mean.

Decision D1 depends on the hyper1 rows, which are still missing. The Mac rows are only a sanity reference: Apple
Silicon has several times the memory bandwidth of the x86 hosts and runs candle's NEON kernels, not AVX2.

## Results

| host | cpu | threads | simd | stage | frames | mean ms | p50 ms | p99 ms | max ms | RTF |
|---|---|---|---|---|---|---|---|---|---|---|
| mac-dev | Apple M4 Pro | 4 | neon | mimi encode | 250 | 14.17 | 14.07 | 15.95 | 19.85 | 5.65 |
| mac-dev | Apple M4 Pro | 4 | neon | mimi decode | 250 | 13.88 | 13.56 | 23.61 | 23.79 | 5.76 |
| mac-dev | Apple M4 Pro | 4 | neon | whole frame | 250 | 28.05 | 27.65 | 37.90 | 38.11 | 2.85 |
| mac-dev | Apple M4 Pro | 1 | neon | mimi encode | 250 | 27.22 | 25.63 | 55.35 | 56.16 | 2.94 |
| mac-dev | Apple M4 Pro | 1 | neon | mimi decode | 250 | 26.24 | 24.68 | 45.16 | 46.93 | 3.05 |
| mac-dev | Apple M4 Pro | 1 | neon | whole frame | 250 | 53.46 | 50.60 | 81.31 | 92.62 | 1.50 |

Measured 2026-09-21, release build, synthetic input, machine otherwise in normal desktop use.

Still to measure:

- hyper1 (i5-11400T), Mimi only, 3 and 4 threads. The M1 target is p99 under 40 ms for encode + decode on 3 cores.
- hyper1, Moshi 7B q8. Expected 220 to 330 ms per step, which would confirm that it is ruled out.
- hyper1, LFM2.5-Audio-1.5B through its x64 runner. `voice-bench` does not cover this; it needs the vendor runner.

## Running it

```
cargo build --release -p voice-bench
./target/release/voice-bench --label hyper1 --threads 3 --header            # Mimi only
./target/release/voice-bench --label hyper1 --threads 3 --bench moshi       # adds the Moshi 7B q8 step
```

Rows go to stdout, progress to stderr, so `>> rows.md` collects a clean table. `--wav-in` replaces the synthetic
signal with a 24 kHz mono 16-bit file. Weights come from the Hugging Face cache or are downloaded on first use:
about 385 MB for Mimi, about 8 GB for Moshi q8, which also needs roughly 10 GB of RAM. `--mimi-model` and
`--moshi-model` take local paths instead.

### On hyper1

A one-off Job, pinned to hyper1 and sized like the planned pod. It is a manual debugging run, not part of the
GitOps tree: create it, read the rows from the log, delete it. Replace the tag with a real CI build.

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: voice-bench
  namespace: innestemme
spec:
  backoffLimit: 0
  ttlSecondsAfterFinished: 86400
  template:
    spec:
      restartPolicy: Never
      affinity:
        nodeAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
            nodeSelectorTerms:
              - matchExpressions:
                  - key: topology.kubernetes.io/zone
                    operator: In
                    values: [hyper1]
      containers:
        - name: voice-bench
          image: ghcr.io/mortennordbye/innestemme:sha-0000000
          command: ["/usr/local/bin/voice-bench"]
          args: ["--label", "hyper1", "--header"]
          env:
            - name: VOICE_THREADS
              value: "3"
          resources:
            requests: { cpu: "3", memory: 2Gi }
            limits: { cpu: "3", memory: 2Gi }
          volumeMounts:
            - name: models
              mountPath: /models
      volumes:
        - name: models
          emptyDir: {}
```

For `--bench moshi` raise memory to 12Gi and give the `emptyDir` room for 9 GB. The namespace's
CiliumNetworkPolicy selects `app: innestemme` only, so this pod's download is not restricted by it.

On x86 the `simd` column must show `avx`. If it shows `scalar`, the binary was built without
`-C target-cpu=x86-64-v3` and the numbers are meaningless.

The `--bench moshi` path compiles but has not been executed yet. The development Mac did not have the disk space
for the weights.

## Kyutai STT-1B (assistant), Mac only so far

`voice-bench --bench stt --wav-in <spoken wav>` times one streaming step (its own 32-codebook Mimi encode plus the
1B LM) and prints the transcript on stderr. Local numbers from 2026-09-23. The homelab CPUs are slower than
this Mac, so these rows are an upper bound, not a prediction for hyper1.

| host | cpu | threads | simd | stage | frames | mean ms | p50 ms | p99 ms | max ms | RTF |
|---|---|---|---|---|---|---|---|---|---|---|
| mac | Apple M4 Pro | 4 | neon | stt-1b step (mimi 32 cb encode + lm) | 100 | 88.98 | 87.87 | 111.33 | 113.24 | 0.90 |
| mac | Apple M4 Pro | 8 | neon | stt-1b step (mimi 32 cb encode + lm) | 60 | 89.62 | 89.01 | 104.06 | 104.06 | 0.89 |
| mac | Apple M4 Pro | 10 | neon | stt-1b step (mimi 32 cb encode + lm) | 60 | 92.11 | 91.90 | 96.66 | 96.66 | 0.87 |
| mac | Apple M4 Pro | 4 | metal | stt-1b step (mimi 32 cb encode + lm) | 100 | 20.20 | 19.41 | 43.02 | 64.08 | 3.96 |

More threads do not help on the CPU. The weights load as f32 (bf16 on disk), so every step reads about 4 GB, and
memory bandwidth is the likely limit. Quantized weights are the first thing to try for the cluster.

## Kyutai Pocket TTS (assistant's English voice), Mac only so far

`voice-bench --bench pocket --precision f32|q8 --threads N --frames 5`: a two-sentence reply (8.5 s of audio),
5 runs after a warm-up, voice "alba". "first audio" is from the call to the first 80 ms chunk; RTF columns are
not meaningful for these rows, the speed is the stderr line. Local numbers from 2026-09-23.

| host | cpu | threads | precision | stage | runs | mean ms | p50 ms | p99 ms | max ms | x real time |
|---|---|---|---|---|---|---|---|---|---|---|
| mac | Apple M4 Pro | 1 | f32 | whole utterance / first audio | 5 | 2343 / 109 | | | | 3.6 |
| mac | Apple M4 Pro | 2 | f32 | whole utterance / first audio | 5 | 1413 / 61 | | | | 6.0 |
| mac | Apple M4 Pro | 3 | f32 | whole utterance / first audio | 5 | 1153 / 45 | | | | 7.4 |
| mac | Apple M4 Pro | 1 | q8 | whole utterance / first audio | 5 | 1495 / 53 | | | | 5.8 |
| mac | Apple M4 Pro | 2 | q8 | whole utterance / first audio | 5 | 965 / 31 | | | | 9.0 |
| mac | Apple M4 Pro | 3 | q8 | whole utterance / first audio | 5 | 857 / 24 | | | | 10.2 |

## Language model fallback (2026-09-24, M4 Pro, Ollama 0.33.3 in OrbStack, 8 threads, CPU)

12 requests the rules miss (`cargo run --release -p voice-assistant --example llm_eval`): skill calls
(weather follow-up, lights, music, joke), ignoring talk meant for someone else, general questions in
English and Norwegian.

| Model | Right | Mean | Notes |
|---|---|---|---|
| qwen3:1.7b | 7-8/12 | 0.3-0.4 s | describes actions instead of calling tools, invents facts |
| qwen3:4b (thinking) | 0/12 | 3.6 s | thinks aloud, ignores `reasoning_effort` |
| qwen3:4b-instruct | 12/12 | 0.48 s | chosen |

- llama.cpp with its default thread count (the VM's 14 cores) ran at 0.5 tokens/s; 8 threads gave
  ~140 tokens/s. Set `num_thread` to the CPU limit in containers.
- The system prompt and tools must not change between requests (no clock, no language line), or
  the prompt cache misses: a follow-up took 4.2 s before and 0.87 s after.

## Name matching (2026-09-24, whisper-base, `say` voices, 44 utterances with the user's names)

| Approach | Names right | Per utterance |
|---|---|---|
| plain | 15/44 | 704 ms |
| vocabulary prompt, 79 / 149 / 222 tokens | 21 / 23 / 25 | 931 / 1155 / 1351 ms |
| plain + sound-alike matching against known names | 33/44, 0 wrong | 691 ms |

## Kokoro TTS (English voice `am_onyx`), 2026-10-10

Kokoro-FastAPI CPU v0.2.4, a sentence synthesized whole (raw PCM, streamed but delivered at the end):

| Where | Threads / CPUs | "It's 4 degrees and rain in Oslo, sir." | departures sentence (4.6 s of audio) |
| --- | --- | --- | --- |
| M4 Pro, Docker | default / 6 | ~0.4 s | ~0.6 s |
| i7-8700T node, Kubernetes | 8 (PyTorch default) / 2 | 18 to 26 s for short whole answers | - |
| i7-8700T node, Kubernetes | 4 / 4 | 2.6 s | 3.6 s |

PyTorch starts one thread per node core; set `OMP_NUM_THREADS` to the CPU limit. With `speech-cache`, fixed
sentences cost nothing after the first time and a lead-in plays while the data sentence is synthesized:
on the Mac, first audio 0.2 to 0.4 ms after transcription for weather, a joke and departures.

## Whisper window on real satellite recordings, 2026-10-10

`whisper_window` on five Voice PE recordings (whisper-base, Metal): the full window took 378 to 498 ms and gave
sane text; `3/10` took 160 to 607 ms and turned one request into "What are you doing?" repeated, another into
"BELL", and "today" into "tonight"; `2/0` changed four of five. The full window is the default since then.


## Speech to text on the Voice PE's utterances, 2026-10-17

156 utterances the engine cut from the living room Voice PE (`target/utterances/utterance-*.wav`, 23 min),
4 CPU threads on an M4 Pro. Reference: whisper-large-v3-turbo (mlx, Metal). "Same intent": of the 94
utterances whose reference parses to a request, how many parse to the identical intent (light and scene
targets compare the raw text, so this undercounts for every system alike). Others with
`examples/stt_eval.rs` (the engine's Whisper) and sherpa-onnx 1.13 (int8 models); scored with `examples/stt_score.rs`.

| System | WER | Same intent | Median | p90 |
|---|---|---|---|---|
| whisper-base, candle f32, full window (the engine today) | 58% | 58/94 (62%) | 558 ms | 632 ms |
| whisper-base, sherpa-onnx int8 | 51% | 61/94 (65%) | 90 ms | 146 ms |
| moonshine-tiny | 77% | 30/94 (32%) | 13 ms | 32 ms |
| moonshine-base | 64% | 36/94 (38%) | 21 ms | 39 ms |
| parakeet-tdt-0.6b-v2 | 43% | 67/94 (71%) | 70 ms | 151 ms |
| zipformer streaming en 2023-06-26 (after the last frame) | 73% | 38/94 (40%) | 7 ms | 8 ms |

- The cluster's engine takes 3.3 s median for the same Whisper (the logs, 63 utterances), about 6x the Mac;
  at that ratio Parakeet would take about 0.4 s there.
- Parakeet's errors against the reference are partly the reference's: it heard "Pause the music", "Tell me
  a joke" and "next bus to Majorstua" where large-v3-turbo wrote "Pulse the music", "The mid-joke" and "next
  best to Majora". On room noise it returns nothing, where the engine's Whisper writes whole sentences ("I'm
  going to play a little bit more"). It writes numbers as words ("thirty percent").
- Moonshine returns nothing for many of these recordings (its own test file works; the level is not the cause).
- The streaming zipformer keeps up easily (real-time factor 0.02) but is the least accurate after Moonshine.

## End to end in the cluster with Parakeet, 2026-10-17

Requests spoken by Home Assistant's Piper voice on the living room Sonos, heard by the Voice PE, answered on
the same Sonos (image 0.0.1792247630, 4 CPUs). From the engine log: `spoken ... after_speech` (end of speech
to the first answer audio) and `answer heard on the device` (announcement to the device's microphone
hearing the answer).

| | Whisper base (12-17 Oct, real use) | Parakeet |
|---|---|---|
| End of speech to answer audio, fixed answer | 3.3 s median | 0.24 s (acted at the 0.24 s pause), 0.64 s otherwise |
| Answer with a live number (Kokoro) | 5-6 s | 2-4 s, then cached for the frequent shapes |
| Announcement to sound in the room | | 0.41 s (5 answers, 0.41-0.49 s) |

So a cached answer is heard about 0.65-1.05 s after the end of speech. The synthetic voice through a speaker
was misheard often (9 of 20: "Stop the camera.", "Set a towel for 10 minutes", "Tell me a fact."); the
benchmark above on real recordings is the better guide to accuracy. Hotwords (sherpa-onnx contextual biasing,
with a `bpe.vocab` made from `tokens.txt`) and snapping unknown requests to catalogue phrases by sound key
did not help on reverberant noisy command recordings, so neither is used.

## The name in the wake word, 2026-10-17

`examples/wake_text.rs`: Parakeet on the 1.5 s before the wake word's peak score and 0.2 s after,
in the 138 Voice PE runs in `target/utterances` (about 120 ms each on an M4 Pro). Of the 70 runs where
`hey_jarvis` scored 0.5 or more, 63 had "Jarvis" or a close spelling ("Jarris", "Jervis"). Of the other
seven, two or three were the TV ("That's my parents.") and three or four were real requests heard
without the name ("Hey Joe's, what's the time today?", "Stop the music."). So the check is no gate
for requests (it would drop about one real wake in twenty); it only decides whether an unrecognised
request gets "didn't catch that" or silence.
