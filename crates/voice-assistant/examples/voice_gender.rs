//! How female each recording's voice sounds (0 male, 1 female) and what the persona would make of
//! it. Satellite runs (`dump-utterances`, 16 kHz) open with the two seconds before the wake word
//! fired; `--wake` scores only the end of that, as the engine does:
//! `cargo run --release -p voice-assistant --example voice_gender -- --wake target/utterances/run-*.wav`.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use voice_assistant::voice_gender::{sure, VoiceGender, SAMPLE_RATE};

/// What the satellite keeps before the wake word, and the end of it the engine classifies.
const PREROLL: usize = 2 * SAMPLE_RATE as usize;
const WAKE: usize = 3 * SAMPLE_RATE as usize / 2;

fn main() -> Result<()> {
    let mut paths: Vec<String> = std::env::args().skip(1).collect();
    let wake = paths.first().is_some_and(|a| a == "--wake");
    if wake {
        paths.remove(0);
    }
    if paths.is_empty() {
        bail!("usage: voice_gender [--wake] <wav>...");
    }
    let model = VoiceGender::load(1)?;
    let (mut took, mut runs) = (Duration::ZERO, 0);
    for path in paths {
        let mut reader = hound::WavReader::open(&path)?;
        let spec = reader.spec();
        if spec.sample_rate != SAMPLE_RATE || spec.channels != 1 {
            bail!("{path}: {} Hz, {} channels; 16 kHz mono expected", spec.sample_rate, spec.channels);
        }
        let pcm: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;
        let pcm = match wake {
            true => &pcm[PREROLL.min(pcm.len()).saturating_sub(WAKE)..PREROLL.min(pcm.len())],
            false => &pcm[..],
        };
        let start = Instant::now();
        match model.female(pcm) {
            Ok(female) => {
                took += start.elapsed();
                runs += 1;
                println!("{female:.2} {:<7} {path}", format!("{:?}", sure(female)));
            }
            Err(error) => println!("  -  {error:#}  {path}"),
        }
    }
    if runs > 0 {
        eprintln!("{:.1} ms per clip", took.as_secs_f64() * 1000.0 / runs as f64);
    }
    Ok(())
}
