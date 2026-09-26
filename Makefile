.PHONY: help tools setup build piper piper-stop ollama assistant log test lint

# The assistant target runs STT on the Mac GPU; see README "Local voice assistant".
FEATURES := voice-engine/metal,voice-bench/metal

help:
	@echo "make setup      build (release, Metal) and download the models (~550 MB, once)"
	@echo "                lights: put HA_URL and HA_TOKEN in .env (see README)"
	@echo "make assistant  run the voice assistant on this Mac's microphone"
	@echo "                settings: config.local.yaml (copy config.example.yaml); the options below override it"
	@echo "                options: NORWEGIAN=false|true LISTENER=whisper|kyutai"
	@echo "                         WHISPER=base|small (small: more accurate, 970 MB download)"
	@echo "                         VOICE=cosette (default) alba fantine eponine azelma marius javert jean"
	@echo "                         POCKET_PRECISION=f32|q8 THREADS=3 ENGLISH_TTS=pocket|say"
	@echo "                         SPEAKER="Living Room" (Music Assistant player, for music)"
	@echo "                         SECONDS_LIVE=600 HOME_PLACE=Oslo SAY_VOICE=Samantha SAY_VOICE_NO=Nora"
	@echo "make piper      run Piper (Norwegian speech) in Docker on 127.0.0.1:10200; set piper: in config"
	@echo "make ollama     run Ollama in Docker with the language model (qwen3:4b-instruct, 2.5 GB)"
	@echo "make log        follow the assistant's server log (what it heard, what it answered)"
	@echo "make test       cargo test --workspace"
	@echo "make lint       cargo clippy with warnings as errors"

tools:
	@command -v cargo >/dev/null || { echo "install Rust first: https://rustup.rs" >&2; exit 1; }
	@command -v say >/dev/null || { echo "macOS 'say' not found; the assistant needs macOS" >&2; exit 1; }

setup: build
	@echo "downloading and loading the models (NB-Whisper base, whisper-tiny, Pocket TTS)..."
	./target/release/voice-engine --processor assistant --stt-gpu --download-only
	@echo "setup done; run: make assistant"

build: tools
	cargo build --release -p voice-engine -p voice-client -p voice-bench --features $(FEATURES)

# Piper speech over Wyoming, the same server Home Assistant's Piper add-on runs. Voices are cached.
PIPER_IMAGE := rhasspy/wyoming-piper:2.5.2
piper:
	@docker inspect -f '{{.State.Running}}' innestemme-piper 2>/dev/null | grep -q true && echo "piper already running" || \
	  docker run -d --rm --name innestemme-piper -p 127.0.0.1:10200:10200 \
	    -v $(HOME)/Library/Caches/innestemme/piper:/data $(PIPER_IMAGE) --voice no_NO-talesyntese-medium

piper-stop:
	docker stop innestemme-piper

# The language model behind the rules. llama.cpp sizes its thread pool from the physical cores it
# sees, which oversubscribes a Docker VM (0.5 instead of ~100 tokens/s on an M4 Pro), so the model
# is wrapped with a fixed thread count. KEEP_ALIVE=-1 keeps it loaded: after Ollama's default five
# idle minutes the next question paid for loading it and ~10 s of prompt on the CPU.
OLLAMA_IMAGE := ollama/ollama:0.33.3
OLLAMA_THREADS ?= 8
ollama:
	@docker inspect -f '{{.State.Running}}' innestemme-ollama 2>/dev/null | grep -q true || \
	  docker run -d --name innestemme-ollama -p 127.0.0.1:11434:11434 -e OLLAMA_KEEP_ALIVE=-1 \
	    -v $(HOME)/Library/Caches/innestemme/ollama:/root/.ollama $(OLLAMA_IMAGE)
	@until curl -sf 127.0.0.1:11434/api/version >/dev/null; do sleep 1; done
	docker exec innestemme-ollama sh -c 'ollama pull qwen3:4b-instruct && \
	  printf "FROM qwen3:4b-instruct\nPARAMETER num_thread $(OLLAMA_THREADS)\n" > /tmp/M && \
	  ollama create qwen3-voice:4b-instruct -f /tmp/M'
	@echo "set in config.local.yaml: llm-url: http://127.0.0.1:11434/v1 and llm-model: qwen3-voice:4b-instruct"

assistant: build
	./scripts/assistant.sh

log:
	tail -f target/assistant.log

test:
	cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --workspace --all-targets --features $(FEATURES) -- -D warnings
