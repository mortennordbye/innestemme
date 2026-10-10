//! The highest wake word score in each wav (16 kHz mono), to pick `--wake-threshold` from real
//! recordings (`dump-utterances` keeps each satellite run, wake word included):
//! `cargo run --release -p voice-assistant --example wake_score -- hey_jarvis target/utterances/run-*.wav`.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Result};
use voice_assistant::wakeword::{WakeDetector, WakeModels, CHUNK};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(model) = args.next() else { bail!("usage: wake_score <model> <wav>...") };
    let models = Arc::new(WakeModels::load(&model)?);
    let (mut chunks, mut took) = (0, std::time::Duration::ZERO);
    for path in args {
        let mut reader = hound::WavReader::open(&path)?;
        let spec = reader.spec();
        if spec.sample_rate != 16_000 || spec.channels != 1 {
            bail!("{path}: {} Hz, {} channels; 16 kHz mono expected", spec.sample_rate, spec.channels);
        }
        let pcm: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;
        // A second of silence on both sides, as a stream would have around it.
        let second = vec![0i16; 16_000];
        let mut detector = WakeDetector::new(models.clone());
        let (mut best, mut at, mut offset) = (0.0f32, 0usize, 0usize);
        let start = Instant::now();
        for block in [&second[..], &pcm, &second].into_iter().flat_map(|a| a.chunks(CHUNK)) {
            offset += block.len();
            if let Some(score) = detector.push(block)? {
                if score > best {
                    (best, at) = (score, offset);
                }
            }
        }
        took += start.elapsed();
        chunks += offset / CHUNK;
        let at = at.saturating_sub(16_000) as f32 / 16_000.0;
        println!("{best:.3} at {at:5.2} s  {path}");
    }
    if chunks > 0 {
        eprintln!("{:.2} ms per 80 ms of audio", took.as_secs_f64() * 1000.0 / chunks as f64);
    }
    Ok(())
}
