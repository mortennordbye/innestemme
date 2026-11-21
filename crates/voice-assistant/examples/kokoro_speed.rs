//! Kokoro in the engine (`english-tts: kokoro-onnx`) against Kokoro-FastAPI (`english-tts: kokoro`), on
//! sentences the engine synthesizes live: time to the first audio and to the whole sentence, and seconds
//! of audio per second. `PHONON=<voice>` adds Phonon (`english-tts: phonon`, int8, temperature 0).
//! `cargo run --release -p voice-assistant --example kokoro_speed -- <threads> [server url] [out dir]`
//! The server is `make kokoro`; limit it to as many CPUs (`docker update --cpus`) for a fair comparison.
//! With an out dir, each sentence is written as `onnx-<n>.wav`, `fastapi-<n>.wav` and `phonon-<n>.wav`.

use std::time::Instant;

use anyhow::Result;
use voice_assistant::kokoro::{KokoroFiles, KokoroOnnx, KokoroTts};
use voice_assistant::lang::Lang;
use voice_assistant::phonon::{Phonon, PhononFiles};
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
    let mut phonon = match std::env::var("PHONON") {
        Ok(voice) => {
            voice_assistant::phonon::set_threads(threads);
            let start = Instant::now();
            let files = PhononFiles::fetch(&voice, true)?;
            let phonon = Phonon::load(&files, &voice, 0.0, true)?;
            println!("Phonon {voice} ready in {} ms", start.elapsed().as_millis());
            Some(phonon)
        }
        Err(_) => None,
    };
    println!("{:62} {:>18} {:>18} {:>18}", "sentence (first / whole)", "in engine", "FastAPI", "Phonon");
    for (i, sentence) in SENTENCES.iter().enumerate() {
        let mut row = format!("{sentence:62}");
        let engines: [(&str, Option<&mut dyn Tts>); 3] = [
            ("onnx", Some(&mut ours)),
            ("fastapi", theirs.as_mut().map(|t| t as &mut dyn Tts)),
            ("phonon", phonon.as_mut().map(|t| t as &mut dyn Tts)),
        ];
        for (name, tts) in engines {
            let Some(tts) = tts else {
                row += &format!(" {:>18}", "-");
                continue;
            };
            let (mut pcm, mut first) = (Vec::new(), None);
            let start = Instant::now();
            tts.speak(sentence, Lang::English, &mut |chunk| {
                first.get_or_insert_with(|| start.elapsed());
                pcm.extend_from_slice(chunk)
            })?;
            let took = start.elapsed();
            let seconds = pcm.len() as f32 / 24_000.0;
            let first = first.unwrap_or(took).as_millis();
            row += &format!(" {:>18}", format!("{first}/{} ms {:.1}x", took.as_millis(), seconds / took.as_secs_f32()));
            if let Some(out) = out {
                voice_codec::wav::write_mono(format!("{out}/{name}-{i}.wav"), 24_000, &pcm)?;
            }
        }
        println!("{row}");
    }
    Ok(())
}
