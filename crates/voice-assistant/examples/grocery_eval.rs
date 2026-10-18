//! How Norwegian grocery names said in an English request come out of Parakeet, and whether the
//! shopping list writes them back the Norwegian way: "Add" and "to the shopping list" in an English
//! macOS voice around the item in Nora's Norwegian, then Parakeet, the list's parser and `groceries`.
//! macOS only. `cargo run --release -p voice-assistant --example grocery_eval -- [item ...]`
//! Without items, every grocery the module knows; prints each miss and the totals. LISTENER=whisper
//! uses Whisper told the language is Norwegian (WHISPER=base|small|medium, default small) instead,
//! and ALONE=1 says the item by itself, as after "what should I add?".

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use voice_assistant::groceries::Groceries;
use voice_assistant::norwegian::{Norwegian, NorwegianFiles};
use voice_assistant::parakeet::{Parakeet, ParakeetFiles, DEFAULT_REPO};
use voice_assistant::shopping::{self, ListCommand};
use voice_assistant::whisper::Transcribe;

fn main() -> Result<()> {
    let mut items: Vec<String> = std::env::args().skip(1).collect();
    if items.is_empty() {
        items = voice_assistant::groceries::names().iter().map(|n| n.to_string()).collect();
    }
    let dir = std::env::temp_dir().join(format!("grocery-eval-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let whisper = std::env::var("LISTENER").is_ok_and(|l| l == "whisper");
    let alone = std::env::var("ALONE").is_ok_and(|a| a == "1");
    let mut parakeet = Parakeet::load(&ParakeetFiles::fetch(DEFAULT_REPO)?, 4)?;
    let norwegian = match whisper {
        true => {
            let size = std::env::var("WHISPER").unwrap_or_else(|_| "small".into());
            Some(Norwegian::load(&NorwegianFiles::fetch(&format!("csukuangfj/sherpa-onnx-whisper-{size}"))?, 4)?)
        }
        false => None,
    };
    let groceries = Groceries::new(Vec::<String>::new());
    let (mut heard_right, mut written_right, mut took) = (0, 0, Duration::ZERO);
    for item in &items {
        let pcm = match alone {
            true => say(&dir, "Nora", item)?,
            false => [
                say(&dir, "Samantha", "Add")?,
                say(&dir, "Nora", item)?,
                say(&dir, "Samantha", "to the shopping list.")?,
            ]
            .concat(),
        };
        let start = Instant::now();
        let text = match &norwegian {
            Some(norwegian) => norwegian.transcribe(&pcm),
            None => parakeet.transcribe(&pcm)?.first().map(|t| t.text.clone()).unwrap_or_default(),
        };
        took += start.elapsed();
        let heard = match (alone, shopping::parse(&text)) {
            (true, _) => shopping::items_of(&text).join(", "),
            (false, Some(ListCommand::Add(found))) => found.join(", "),
            (false, _) => String::new(),
        };
        let written = groceries.norwegian(&heard);
        let same = |a: &str| a.to_lowercase() == item.to_lowercase();
        heard_right += usize::from(same(&heard));
        written_right += usize::from(same(&written));
        if !same(&written) {
            println!("{item:22} heard {text:?} -> {heard:?} -> {written:?}");
        }
    }
    let n = items.len();
    println!(
        "{n} items: heard as written {heard_right} ({}%), on the list as Norwegian {written_right} ({}%), {} ms each",
        heard_right * 100 / n.max(1),
        written_right * 100 / n.max(1),
        took.as_millis() / n.max(1) as u128
    );
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// `text` in `voice`, as 24 kHz mono samples.
fn say(dir: &std::path::Path, voice: &str, text: &str) -> Result<Vec<i16>> {
    let (aiff, wav) = (dir.join("say.aiff"), dir.join("say.wav"));
    let ok = std::process::Command::new("say").args(["-v", voice, "-o"]).arg(&aiff).arg(text).status()?.success()
        && std::process::Command::new("afconvert")
            .args(["-f", "WAVE", "-d", "LEI16@24000", "-c", "1"])
            .arg(&aiff)
            .arg(&wav)
            .status()?
            .success();
    anyhow::ensure!(ok, "say or afconvert failed for {text:?}");
    hound::WavReader::open(&wav)?.samples::<i16>().collect::<Result<_, _>>().context("reading the wav")
}
