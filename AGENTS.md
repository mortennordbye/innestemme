# AGENTS.md

Guidance for coding agents (Claude Code and others) working in this repository.

## Commands

| Task | Command |
| ---- | ------- |
| Build (Mac, Metal) and download models | `make setup` |
| Build | `cargo build --release -p voice-engine -p voice-client` |
| Test | `make test` (`cargo test --workspace`) |
| Lint | `make lint` (`cargo clippy --workspace --all-targets -- -D warnings`) |
| Format | `rustfmt --edition 2021 <changed files>`; a plain `cargo fmt` also rewrites a few hand-formatted spots |
| Run the assistant locally | `make ollama`, then `make assistant` (settings in `config.local.yaml`) |
| One home request without audio | `cargo run -p voice-assistant --example home -- "what's on the shopping list"` |
| Language model accuracy and latency | `cargo run --release -p voice-assistant --example llm_eval -- http://127.0.0.1:11434/v1 qwen3-voice:4b-instruct` |
| Container image | `docker build .` |

## Layout

- `crates/voice-proto/` — wire format for the UDP audio protocol
- `crates/voice-rt/` — real-time plumbing: jitter buffer, histograms, allocation guard, thread pinning
- `crates/voice-codec/` — PCM packets and the Mimi codec
- `crates/voice-engine/` — the server: transport, pipeline, assistant worker, ESPHome satellite bridge, settings
- `crates/voice-assistant/` — speech-to-text, wake word, intent rules, language model client, skills, speech output
- `crates/voice-esphome/` — ESPHome native API client (plaintext and Noise)
- `crates/voice-client/` — test client: tone, wav or live microphone
- `crates/voice-bench/` — benchmark tool; results in `docs/benchmarks.md`

## Conventions

- New skills get rules in English and Norwegian (`intent.rs` or their own module), a language model tool in
  `llm.rs` when phrasing varies, and a case in `examples/llm_eval.rs`. Keep tool descriptions short: every token
  is in every request and slows answers on a CPU.
- Personal values (addresses, Home Assistant URL and token, names) belong in `config.local.yaml` or `.env`, both
  git-ignored, never in code, tests or docs.
- Comments state constraints and reasons, not what the next line does.
- No AI attribution in commit messages (no `Co-authored-by` for agents, no session links); CI rejects them.
- Say so before anything plays on real speakers.
- Never commit secrets or credentials.
