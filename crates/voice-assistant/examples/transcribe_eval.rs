//! Measures recognition of names: transcribes each case with and without a vocabulary prompt.
//! `cargo run --release -p voice-assistant --example transcribe_eval -- cases.tsv vocab.txt 0,80,150,223`
//! cases.tsv: `<24 kHz wav>\t<name that must appear>` per line; vocab.txt: one name per line.

use std::time::Instant;

use anyhow::{Context, Result};
use voice_assistant::dialog::normalize;
use voice_assistant::lang::Lang;
use voice_assistant::names::Names;
use voice_assistant::whisper::{default_repo, Transcriber, WakePrompt, WhisperFiles};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [cases, vocab, budgets] = args.as_slice() else {
        anyhow::bail!("usage: transcribe_eval cases.tsv vocab.txt 0,80,150")
    };
    let cases: Vec<(String, String)> = std::fs::read_to_string(cases)?
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(a, b)| (a.to_owned(), b.to_owned())))
        .collect();
    let vocab: Vec<String> = std::fs::read_to_string(vocab)?.lines().map(str::to_owned).collect();
    let names = Names::new(&vocab);
    // Controls: sentences whose "name" is not in the vocabulary must not match anything.
    let known = |name: &str| vocab.iter().any(|v| v.to_lowercase().contains(&name.to_lowercase()));
    let files = WhisperFiles::fetch(&default_repo(false, "base"))?;
    let audio: Vec<Vec<i16>> = cases
        .iter()
        .map(|(path, _)| -> Result<Vec<i16>> {
            Ok(hound::WavReader::open(path)
                .with_context(|| path.clone())?
                .samples::<i16>()
                .collect::<Result<_, _>>()?)
        })
        .collect::<Result<_>>()?;
    for budget in budgets.split(',').map(|b| b.parse::<usize>()) {
        let budget = budget?;
        let mut t = Transcriber::load(&files, None, voice_assistant::stt::device(false)?)?
            .with_wake_prompt(WakePrompt::Context, "Homie");
        t.restrict(Lang::English);
        if budget > 0 {
            t = t.with_vocabulary(&vocab, budget);
        }
        t.transcribe(&[0i16; 24_000])?;
        let (mut hits, mut wakes, mut took, mut matched, mut wrong) = (0, 0, 0.0, 0, 0);
        let mut misses = Vec::new();
        for ((path, name), pcm) in cases.iter().zip(&audio) {
            let start = Instant::now();
            let text = t.transcribe(pcm)?.first().map(|c| c.text.clone()).unwrap_or_default();
            took += start.elapsed().as_secs_f64();
            let squash = |s: &str| s.split_whitespace().map(normalize).collect::<String>();
            if squash(&text).contains(&squash(name)) {
                hits += 1;
            } else {
                misses.push(format!("{:<34} {name:<18} -> {text}", path.rsplit('/').next().unwrap_or(path)));
            }
            // The request after the name, matched against the known names.
            let request = text.split_once([',', '.']).map_or(text.as_str(), |(_, r)| r);
            let found = names.best(request).map(|m| m.name.to_owned());
            match (&found, known(name)) {
                (Some(f), true) if f.to_lowercase().contains(&name.to_lowercase()) => matched += 1,
                (None, false) => matched += 1,
                (f, _) => {
                    wrong += usize::from(f.is_some());
                    misses.push(format!(
                        "{:<34} {name:<18} match {f:?} <- {request}",
                        path.rsplit('/').next().unwrap_or(path)
                    ));
                }
            }
            if normalize(text.split_whitespace().next().unwrap_or_default()).starts_with("homie") {
                wakes += 1;
            }
        }
        println!(
            "prompt {:>3} tokens: names {hits}/{n} as heard, {matched}/{n} after matching ({wrong} wrong), wake word {wakes}/{n}, {:.0} ms per utterance",
            t.prompt_tokens(),
            took * 1000.0 / cases.len() as f64,
            n = cases.len()
        );
        for m in misses {
            println!("    miss {m}");
        }
    }
    Ok(())
}
