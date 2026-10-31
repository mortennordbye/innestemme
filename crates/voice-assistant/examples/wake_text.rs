//! What Parakeet hears in the wake word of each satellite recording (16 kHz mono, `dump-utterances`
//! runs), to check that a false wake can be told from a real one by the name in it:
//! `cargo run --release -p voice-assistant --example wake_text -- hey_jarvis target/utterances/run-*.wav`.
//! PARAKEET=<model> picks another Parakeet model (default the engine's).

use std::sync::Arc;

use anyhow::{bail, Result};
use voice_assistant::parakeet::{Parakeet, ParakeetFiles, DEFAULT_REPO};
use voice_assistant::resample::to_24k;
use voice_assistant::wakeword::{WakeDetector, WakeModels, CHUNK};
use voice_assistant::whisper::Transcribe;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(model) = args.next() else { bail!("usage: wake_text <model> <wav>...") };
    let models = Arc::new(WakeModels::load(&model)?);
    let mut parakeet =
        Parakeet::load(&ParakeetFiles::fetch(&std::env::var("PARAKEET").unwrap_or_else(|_| DEFAULT_REPO.into()))?, 4)?;
    let before = std::env::var("BEFORE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500usize) * 16;
    let after = std::env::var("AFTER_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(200usize) * 16;
    for path in args {
        let mut reader = hound::WavReader::open(&path)?;
        let pcm: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;
        let mut detector = WakeDetector::new(models.clone());
        let (mut best, mut at, mut offset) = (0.0f32, 0usize, 0usize);
        for block in pcm.chunks(CHUNK) {
            offset += block.len();
            if let Some(score) = detector.push(block)? {
                if score > best {
                    (best, at) = (score, offset);
                }
            }
        }
        let wake = &pcm[at.saturating_sub(before)..(at + after).min(pcm.len())];
        let audio = to_24k(wake, 16_000);
        let start = std::time::Instant::now();
        let text = parakeet.transcribe(&audio)?.first().map(|t| t.text.clone()).unwrap_or_default();
        let name = path.rsplit('/').next().unwrap_or(&path);
        println!("{best:.2}\t{}ms\t{name}\t{text}", start.elapsed().as_millis());
    }
    Ok(())
}
