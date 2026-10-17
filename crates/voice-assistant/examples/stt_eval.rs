//! Times the engine's Whisper on recorded utterances, set up as the engine sets it up (English,
//! context prompt), and prints `<file>\t<ms>\t<text>` per utterance.
//! `RAYON_NUM_THREADS=4 cargo run --release -p voice-assistant --example stt_eval -- target/utterances/utterance-*.wav`
//! WHISPER_SIZE picks the model (default base); WINDOW=short uses the short window instead of the full one.

use std::time::Instant;

use anyhow::{Context, Result};
use voice_assistant::lang::Lang;
use voice_assistant::whisper::{default_repo, Transcriber, WakePrompt, WhisperFiles, Window};

fn main() -> Result<()> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    let size = std::env::var("WHISPER_SIZE").unwrap_or_else(|_| "base".into());
    let window = (std::env::var("WINDOW").as_deref() == Ok("short")).then_some(Window::DEFAULT);
    let files = WhisperFiles::fetch(&default_repo(false, &size))?;
    let mut t = Transcriber::load(&files, None, voice_assistant::stt::device(false)?)?
        .with_wake_prompt(WakePrompt::Context, "Jarvis")
        .with_window(window);
    t.restrict(Lang::English);
    // The first call sets up buffers; it is not what a request costs.
    t.transcribe(&[0i16; 24_000])?;
    for path in &paths {
        let mut reader = hound::WavReader::open(path).with_context(|| path.clone())?;
        anyhow::ensure!(reader.spec().sample_rate == 24_000, "{path}: not 24 kHz");
        let pcm: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;
        let start = Instant::now();
        let text = t.transcribe(&pcm)?.first().map(|c| c.text.clone()).unwrap_or_default();
        let name = path.rsplit('/').next().unwrap_or(path);
        println!("{name}\t{}\t{}", start.elapsed().as_millis(), text.replace(['\t', '\n'], " "));
    }
    Ok(())
}
