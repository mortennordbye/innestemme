//! Utterance transcription with NVIDIA Parakeet TDT 0.6B v2 (int8, through sherpa-onnx): English
//! only. On the Voice PE's recordings it was the most accurate of the models tried and about 8x
//! faster than whisper-base on a CPU, and it stays quiet on room noise where Whisper writes
//! sentences (docs/benchmarks.md). Not streaming: one call per finished utterance, like Whisper.

use std::path::PathBuf;

use anyhow::{Context, Result};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};

use crate::lang::Lang;
use crate::whisper::{resample_24k_to_16k, Transcribe, Transcript};

pub const DEFAULT_REPO: &str = "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8";

/// The model files, fetched from Hugging Face into its cache.
pub struct ParakeetFiles {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

impl ParakeetFiles {
    pub fn fetch(repo: &str) -> Result<Self> {
        let api = hf_hub::api::sync::ApiBuilder::from_env().build()?;
        let repo_api = api.model(repo.to_owned());
        let get = |file: &str| repo_api.get(file).with_context(|| format!("fetching {repo}/{file}"));
        Ok(Self {
            encoder: get("encoder.int8.onnx")?,
            decoder: get("decoder.int8.onnx")?,
            joiner: get("joiner.int8.onnx")?,
            tokens: get("tokens.txt")?,
        })
    }
}

pub struct Parakeet {
    recognizer: OfflineRecognizer,
}

impl Parakeet {
    pub fn load(files: &ParakeetFiles, threads: usize) -> Result<Self> {
        let path = |p: &PathBuf| Some(p.to_string_lossy().into_owned());
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer = OfflineTransducerModelConfig {
            encoder: path(&files.encoder),
            decoder: path(&files.decoder),
            joiner: path(&files.joiner),
        };
        config.model_config.tokens = path(&files.tokens);
        config.model_config.model_type = Some("nemo_transducer".into());
        config.model_config.num_threads = threads.max(1) as i32;
        let recognizer = OfflineRecognizer::create(&config).context("loading Parakeet")?;
        Ok(Self { recognizer })
    }
}

impl Transcribe for Parakeet {
    /// One English transcript, or none for an utterance without words.
    fn transcribe(&mut self, pcm: &[i16]) -> Result<Vec<Transcript>> {
        let stream = self.recognizer.create_stream();
        // At the model's own rate: given 24 kHz, sherpa-onnx resamples too, but logs it every time.
        stream.accept_waveform(16_000, &resample_24k_to_16k(pcm));
        self.recognizer.decode(&stream);
        let text = stream.get_result().map(|r| r.text).unwrap_or_default();
        let text = digits(text.trim());
        if text.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![Transcript { lang: Lang::English, text, score: 0.0 }])
    }
}

const UNITS: &[&str] = &[
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: &[&str] = &["twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

/// Numbers written out as digits, as Whisper writes them: Parakeet says "thirty percent" where the
/// rules look for "30 percent". "One" on its own stays a word ("which one"), as do numbers inside
/// other words.
pub fn digits(text: &str) -> String {
    let words: Vec<&str> = text.split(' ').collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        match number_at(&words[i..]) {
            Some((value, used, tail)) if !(used == 1 && value == 1) => {
                out.push(format!("{value}{tail}"));
                i += used;
            }
            _ => {
                out.push(words[i].to_owned());
                i += 1;
            }
        }
    }
    out.join(" ")
}

/// The number the words start with: its value, how many words it took, and the punctuation after it.
fn number_at(words: &[&str]) -> Option<(u32, usize, String)> {
    let value = |w: &str| {
        UNITS
            .iter()
            .position(|u| *u == w)
            .map(|n| n as u32)
            .or_else(|| TENS.iter().position(|t| *t == w).map(|n| 20 + 10 * n as u32))
    };
    let (mut total, mut current, mut used, mut tail) = (0u32, 0u32, 0usize, String::new());
    for (i, raw) in words.iter().enumerate() {
        let word = raw.trim_end_matches(|c: char| c.is_ascii_punctuation());
        let lower = word.to_lowercase();
        let n = match lower.split_once('-') {
            // "Twenty-five".
            Some((tens, unit)) => match (value(tens), value(unit)) {
                (Some(t), Some(u)) if t >= 20 && u < 10 => Some(t + u),
                _ => None,
            },
            None => value(&lower),
        };
        let started = used > 0;
        match n {
            // "Twenty five", "one hundred twenty", "two thousand five" add up; "five twenty" is two numbers.
            Some(n)
                if !started
                    || (current == 0 && total > 0)
                    || (current >= 100 && current % 100 == 0)
                    || (n < 10 && current % 100 >= 20 && current % 10 == 0) =>
            {
                current += n
            }
            None if started && lower == "hundred" && current < 10 => current *= 100,
            None if started && lower == "thousand" && total == 0 => (total, current) = ((total + current) * 1000, 0),
            // "One hundred and twenty": the "and" belongs to the number when one follows.
            None if started && lower == "and" && current >= 100 && tail.is_empty() => continue,
            _ => break,
        }
        used = i + 1;
        tail = raw[word.len()..].to_owned();
        if !tail.is_empty() {
            break;
        }
    }
    (used > 0).then(|| (total + current, used, tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_numbers_as_digits() {
        assert_eq!(digits("Put the music at thirty percent"), "Put the music at 30 percent");
        assert_eq!(digits("Set a timer for twenty five minutes."), "Set a timer for 25 minutes.");
        assert_eq!(digits("Set a timer for twenty-five minutes."), "Set a timer for 25 minutes.");
        assert_eq!(digits("one hundred and twenty seconds"), "120 seconds");
        assert_eq!(digits("What's two plus two?"), "What's 2 plus 2?");
        assert_eq!(digits("Volume fifty."), "Volume 50.");
        assert_eq!(digits("Which one?"), "Which one?");
        assert_eq!(digits("One minute."), "One minute.");
        assert_eq!(digits("Set a timer for ten, then nine"), "Set a timer for 10, then 9");
        assert_eq!(digits("Turn on the kitchen"), "Turn on the kitchen");
        assert_eq!(digits("Someone is at the door"), "Someone is at the door");
    }
}
