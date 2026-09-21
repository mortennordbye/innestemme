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
  namespace: ai-voice
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
          image: ghcr.io/mortennordbye/ai-voice:sha-0000000
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
CiliumNetworkPolicy selects `app: ai-voice` only, so this pod's download is not restricted by it.

On x86 the `simd` column must show `avx`. If it shows `scalar`, the binary was built without
`-C target-cpu=x86-64-v3` and the numbers are meaningless.

The `--bench moshi` path compiles but has not been executed yet. The development Mac did not have the disk space
for the weights.
