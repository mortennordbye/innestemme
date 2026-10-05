//! Pocket TTS speed at f32 and q8: time to the first audio and to the whole answer.
//! `cargo run --release -p voice-assistant --example pocket_speed -- cosette 4 [out-dir]`
//! With an output directory, each answer is also written there as `<precision>-<n>.wav`.

use std::time::Instant;

use voice_assistant::lang::Lang;
use voice_assistant::pocket::{self, PocketFiles, Precision};

const ANSWERS: [&str; 3] = [
    "Okay, the living room lights are on.",
    "It's twelve degrees and cloudy right now.",
    "I went to the zoo yesterday and saw a baguette in a cage. It was bread in captivity.",
];

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let voice = args.first().map_or("cosette", String::as_str);
    let threads = args.get(1).map_or(Ok(4), |t| t.parse())?;
    let out = args.get(2).map(std::path::PathBuf::from);
    pocket::set_threads(threads);
    let files = PocketFiles::fetch(voice)?;
    for (name, precision) in [("f32", Precision::F32), ("q8", Precision::Q8)] {
        let mut tts = pocket::load(&files, precision)?;
        tts.speak("Warming up.", Lang::English, &mut |_| {})?;
        println!("{name}");
        for (n, answer) in ANSWERS.iter().enumerate() {
            let start = Instant::now();
            let (mut first, mut audio) = (None, Vec::new());
            // Sentence by sentence, as the engine does.
            for sentence in answer.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty()) {
                tts.speak(sentence, Lang::English, &mut |pcm| {
                    first.get_or_insert_with(|| start.elapsed());
                    audio.extend_from_slice(pcm);
                })?;
            }
            let done = start.elapsed().as_secs_f64() * 1000.0;
            if let Some(dir) = &out {
                let spec = hound::WavSpec {
                    channels: 1,
                    sample_rate: 24_000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                };
                let mut writer = hound::WavWriter::create(dir.join(format!("{name}-{}.wav", n + 1)), spec)?;
                audio.iter().try_for_each(|&s| writer.write_sample(s))?;
                writer.finalize()?;
            }
            let seconds = audio.len() as f32 / 24_000.0;
            let first = first.unwrap_or_default().as_secs_f64() * 1000.0;
            println!("  first {first:>5.0} ms, all {done:>5.0} ms for {seconds:.1} s of audio: {answer}");
        }
    }
    Ok(())
}
