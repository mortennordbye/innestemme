//! Scores transcripts from `stt_eval` (or any `<file>\t<ms>\t<text>` list) against a reference
//! list: word error rate, whether the request parses to the same intent, and time per utterance.
//! `cargo run -p voice-assistant --example stt_score -- reference.tsv whisper.tsv moonshine.tsv ...`

use std::collections::HashMap;

use anyhow::{Context, Result};
use voice_assistant::dialog::normalize;
use voice_assistant::intent::{self, Intent};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (reference, systems) = args.split_first().context("usage: stt_score reference.tsv system.tsv...")?;
    let reference = read(reference)?;
    let request = |text: &str| strip_wake(text);
    println!(
        "{:<24} {:>5} {:>8} {:>10} {:>10} {:>9} {:>9}",
        "system", "WER", "requests", "same", "acted", "median", "p90"
    );
    for path in systems {
        let heard = read(path)?;
        let (mut errors, mut words, mut requests, mut same, mut acted) = (0, 0, 0, 0, 0);
        let mut ms: Vec<u64> = Vec::new();
        for (file, (_, truth)) in &reference {
            let Some((took, text)) = heard.get(file) else { continue };
            ms.push(*took);
            let (truth_words, text_words) = (words_of(truth), words_of(text));
            errors += edit_distance(&truth_words, &text_words);
            words += truth_words.len();
            let wanted = intent::parse(&request(truth));
            let got = intent::parse(&request(text));
            if wanted != Intent::Unknown {
                requests += 1;
                same += usize::from(wanted == got);
            } else if got != Intent::Unknown {
                // Something acted on that the reference did not ask for.
                acted += 1;
            }
        }
        ms.sort_unstable();
        let at =
            |q: f64| ms.get(((ms.len() as f64 * q) as usize).min(ms.len().saturating_sub(1))).copied().unwrap_or(0);
        let name = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".tsv");
        println!(
            "{name:<24} {:>4.0}% {requests:>8} {:>5} {:>3.0}% {acted:>10} {:>7}ms {:>7}ms",
            100.0 * errors as f64 / words.max(1) as f64,
            same,
            100.0 * same as f64 / requests.max(1) as f64,
            at(0.5),
            at(0.9),
        );
    }
    Ok(())
}

fn read(path: &str) -> Result<HashMap<String, (u64, String)>> {
    let text = std::fs::read_to_string(path).with_context(|| path.to_owned())?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let file = parts.next()?.to_owned();
            let ms = parts.next()?.parse().ok()?;
            Some((file, (ms, parts.next().unwrap_or_default().to_owned())))
        })
        .collect())
}

/// The request after "Hey Jarvis," / "Jarvis,", as the engine strips it.
fn strip_wake(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let skip = words.iter().take(2).position(|w| normalize(w) == "jarvis").map_or(0, |i| i + 1);
    words[skip..].join(" ")
}

/// Lowercase words with numbers as digits, so "one minute" and "1 minute" agree.
fn words_of(text: &str) -> Vec<String> {
    const NUMBERS: &[&str] = &["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"];
    text.split_whitespace()
        .map(normalize)
        .filter(|w| !w.is_empty())
        .map(|w| NUMBERS.iter().position(|n| *n == w).map_or(w, |n| n.to_string()))
        .collect()
}

fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let next = (row[j + 1] + 1).min(row[j] + 1).min(diagonal + usize::from(x != y));
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}
