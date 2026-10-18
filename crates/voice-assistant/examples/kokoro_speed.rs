//! Kokoro in the engine (`english-tts: kokoro-onnx`) against Kokoro-FastAPI (`english-tts: kokoro`), on
//! sentences the engine synthesizes live: time to the whole sentence, and seconds of audio per second.
//! `cargo run --release -p voice-assistant --example kokoro_speed -- <threads> [server url] [out dir]`
//! The server is `make kokoro`; limit it to as many CPUs (`docker update --cpus`) for a fair comparison.
//! With an out dir, each sentence is written as `onnx-<n>.wav` and `fastapi-<n>.wav` to compare by ear.

use std::time::Instant;

use anyhow::Result;
use voice_assistant::kokoro::{KokoroFiles, KokoroOnnx, KokoroTts};
use voice_assistant::lang::Lang;
use voice_assistant::tts::Tts;

const SENTENCES: &[&str] = &[
    "It's 23 degrees in the living room, sir.",
    "Okay, the kitchen lights are off.",
    "37 seconds left on the 1 minute timer.",
    "It's 5 degrees and partly cloudy in Oslo, with a high of 13.",
    "Today it ranges from 36 øre to 2.10 kroner.",
    "The next tram is the 18 to Rikshospitalet, in 4 minutes.",
];

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let threads: usize = args.first().map_or(Ok(4), |t| t.parse())?;
    let server = args.get(1).filter(|s| !s.is_empty());
    let out = args.get(2);
    let start = Instant::now();
    let files = KokoroFiles::fetch(voice_assistant::kokoro::ONNX_REPO)?;
    let mut ours = KokoroOnnx::load(&files, "am_onyx", threads)?;
    println!("Kokoro in the engine ready in {} ms, {threads} threads", start.elapsed().as_millis());
    let mut theirs = server.map(|url| KokoroTts::new(url, "am_onyx"));
    println!("{:62} {:>12} {:>12}", "sentence", "in engine", "FastAPI");
    for (i, sentence) in SENTENCES.iter().enumerate() {
        let mut row = format!("{sentence:62}");
        let engines: [(&str, Option<&mut dyn Tts>); 2] =
            [("onnx", Some(&mut ours)), ("fastapi", theirs.as_mut().map(|t| t as &mut dyn Tts))];
        for (name, tts) in engines {
            let Some(tts) = tts else { continue };
            let mut pcm = Vec::new();
            let start = Instant::now();
            tts.speak(sentence, Lang::English, &mut |chunk| pcm.extend_from_slice(chunk))?;
            let took = start.elapsed();
            let seconds = pcm.len() as f32 / 24_000.0;
            row += &format!(" {:>12}", format!("{} ms {:.1}x", took.as_millis(), seconds / took.as_secs_f32()));
            if let Some(out) = out {
                let spec = hound::WavSpec {
                    channels: 1,
                    sample_rate: 24_000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                };
                let mut writer = hound::WavWriter::create(format!("{out}/{name}-{i}.wav"), spec)?;
                pcm.iter().try_for_each(|&s| writer.write_sample(s))?;
                writer.finalize()?;
            }
        }
        println!("{row}");
    }
    Ok(())
}
