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

## No AI traces

Nothing an agent writes may say or show that an AI was involved. This covers commit messages, commit authors,
branch names, tags, pull request and issue titles and bodies, review comments, code comments, docs, the README
and every other file. Specifically, never write:

- Trailers of any kind that name an agent or tool, in any letter case: `Co-authored-by:`, `Co-Authored-By:`,
  `Assisted-by:`, `Generated-by:`, `Signed-off-by:` or `Reviewed-by:` naming Claude, Anthropic, Codex, OpenAI,
  ChatGPT, Copilot, Gemini, Cursor, Jules, Devin, Aider or any other assistant.
- Session or product links and ids: `Claude-Session:`, `claude.ai/...` links, `https://claude.com/claude-code`,
  "Generated with [Claude Code]", "Created with Codex" and the like, as a footer or anywhere else.
- Emoji that signal automation: no 🤖, and no ✨ or similar "generated" markers.
- Phrases such as "made by Claude", "written by AI", "AI-generated", "AI-assisted", "as an AI", "I (Claude)"
  or "this change was generated", in any file, message or comment.
- Agent identities in git: never set an agent's name or an address like `noreply@anthropic.com` as author or
  committer. Commits use the repository owner's configured git identity.
- Agent-named branches (`claude/...`, `codex/...`, `copilot/...`): name branches after the change instead.

If a tool or harness adds any of this by default (a commit trailer, a PR footer, a session link), remove it
before committing or posting; that default does not apply here. CI rejects the unambiguous markers (trailers, session
links, "generated with" or "made by" an agent, 🤖, agent identities) on pull requests and on pushes to `main`;
wording such as "AI-generated", and code, docs and PR bodies, are only guarded by this rule. Removing a trace afterwards means rewriting published history.

## Conventions

- New skills get rules in English and Norwegian (`intent.rs` or their own module), a language model tool in
  `llm.rs` when phrasing varies, and a case in `examples/llm_eval.rs`. Keep tool descriptions short: every token
  is in every request and slows answers on a CPU.
- Personal values (addresses, Home Assistant URL and token, names) belong in `config.local.yaml` or `.env`, both
  git-ignored, never in code, tests or docs.
- Comments state constraints and reasons, not what the next line does.
- Say so before anything plays on real speakers.
- Never commit secrets or credentials.
