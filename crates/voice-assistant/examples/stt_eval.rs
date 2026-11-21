//! Times the engine's Whisper on recorded utterances, set up as the engine sets it up (English,
//! context prompt), and prints `<file>\t<ms>\t<text>` per utterance.
//! `RAYON_NUM_THREADS=4 cargo run --release -p voice-assistant --example stt_eval -- target/utterances/utterance-*.wav`
//! WHISPER_SIZE picks the model (default base); WINDOW=short uses the short window instead of the full one;
//! GPU=1 runs on Metal (build with `--features metal`).

use std::time::Instant;

use anyhow::Result;
use voice_assistant::lang::Lang;
use voice_assistant::whisper::{Transcriber, WakePrompt, Window};

mod common;

fn main() -> Result<()> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    let window = (std::env::var("WINDOW").as_deref() == Ok("short")).then_some(Window::DEFAULT);
    let files = common::whisper_from_env()?;
    let mut t = Transcriber::load(&files, None, voice_assistant::stt::device(std::env::var_os("GPU").is_some())?)?
        .with_wake_prompt(WakePrompt::Context, "Jarvis")
        .with_window(window);
    t.restrict(Lang::English);
    // The first call sets up buffers; it is not what a request costs.
    t.transcribe(&[0i16; 24_000])?;
    for path in &paths {
        let pcm = voice_rt::wav::read_mono(path, 24_000)?;
        let start = Instant::now();
        let text = t.transcribe(&pcm)?.first().map(|c| c.text.clone()).unwrap_or_default();
        let name = path.rsplit('/').next().unwrap_or(path);
        println!("{name}\t{}\t{}", start.elapsed().as_millis(), text.replace(['\t', '\n'], " "));
    }
    Ok(())
}
