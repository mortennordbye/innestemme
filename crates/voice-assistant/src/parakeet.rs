//! Utterance transcription with NVIDIA Parakeet TDT (int8, through sherpa-onnx): English only. On
//! the Voice PE's recordings the 0.6B v2 was the most accurate of the models tried and the 110M
//! nearly as accurate at a quarter of the time; both stay quiet on most room noise where Whisper
//! writes sentences (docs/benchmarks.md). Not streaming: one call per finished utterance.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};

use crate::lang::Lang;
use crate::whisper::{resample_24k_to_16k, Transcribe, Transcript};

/// The 110M transducer: 18 ms per second of audio on an M4 Pro against 66 ms for the 0.6B v2,
/// for 70 of 93 requests understood against 73. Its Hugging Face copy is empty, so it comes from
/// sherpa-onnx's release.
pub const DEFAULT_REPO: &str = "parakeet-110m";
/// The more accurate and slower model, from Hugging Face.
pub const LARGE_REPO: &str = "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8";

const RELEASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models";
/// Release archives by short name: (name, archive stem, SHA-256 of the archive).
const ARCHIVES: &[(&str, &str, &str)] = &[(
    "parakeet-110m",
    "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8",
    "f628312e9fdf8686374cb01a69425c41732529d540860311f16f37cbc32cfe9b",
)];

/// The model files.
pub struct ParakeetFiles {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

impl ParakeetFiles {
    /// `model` is a short name from the sherpa-onnx release (`parakeet-110m`), a local directory, or
    /// a Hugging Face repo with the sherpa-onnx export.
    pub fn fetch(model: &str) -> Result<Self> {
        if let Some(&(_, stem, sha256)) = ARCHIVES.iter().find(|(name, ..)| *name == model) {
            return Ok(Self::in_dir(&unpacked(stem, sha256)?));
        }
        if Path::new(model).is_dir() {
            return Ok(Self::in_dir(Path::new(model)));
        }
        let api = hf_hub::api::sync::ApiBuilder::from_env().build()?;
        let repo_api = api.model(model.to_owned());
        let get = |file: &str| repo_api.get(file).with_context(|| format!("fetching {model}/{file}"));
        Ok(Self {
            encoder: get("encoder.int8.onnx")?,
            decoder: get("decoder.int8.onnx")?,
            joiner: get("joiner.int8.onnx")?,
            tokens: get("tokens.txt")?,
        })
    }

    fn in_dir(dir: &Path) -> Self {
        Self {
            encoder: dir.join("encoder.int8.onnx"),
            decoder: dir.join("decoder.int8.onnx"),
            joiner: dir.join("joiner.int8.onnx"),
            tokens: dir.join("tokens.txt"),
        }
    }
}

/// The release archive `stem`, downloaded, checked and unpacked once into the model cache.
fn unpacked(stem: &str, sha256: &str) -> Result<PathBuf> {
    let root = crate::wakeword::cache_dir().join("sherpa-onnx");
    let dir = root.join(stem);
    // Written last, so a half-unpacked directory is not taken for the model.
    let done = dir.join(".unpacked");
    if done.exists() {
        return Ok(dir);
    }
    let file = format!("{stem}.tar.bz2");
    let archive = crate::wakeword::fetch_from(&root, RELEASE, (&file, sha256))?;
    let reader = std::fs::File::open(&archive).with_context(|| format!("opening {}", archive.display()))?;
    tar::Archive::new(bzip2::read::BzDecoder::new(reader))
        .unpack(&root)
        .with_context(|| format!("unpacking {}", archive.display()))?;
    std::fs::write(&done, b"")?;
    // The archive is only needed until it is unpacked; the models volume is small.
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(dir.join("test_wavs"));
    Ok(dir)
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
        let text = digits(&letters(text.trim()));
        if text.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![Transcript { lang: Lang::English, text, score: 0.0 }])
    }
}

/// Letters spoken one at a time written together, as Whisper writes them: "the T V" is "the TV".
fn letters(text: &str) -> String {
    let single = |w: &str| w.len() == 1 && w.chars().all(|c| c.is_ascii_uppercase()) && w != "I" && w != "A";
    let words: Vec<&str> = text.split(' ').collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    for (i, word) in words.iter().enumerate() {
        let bare = word.trim_end_matches(|c: char| c.is_ascii_punctuation());
        let after_letter = i > 0 && single(words[i - 1]);
        match out.last_mut() {
            Some(last) if after_letter && bare.len() == 1 && bare.chars().all(|c| c.is_ascii_uppercase()) => {
                last.push_str(word)
            }
            _ => out.push((*word).to_owned()),
        }
    }
    out.join(" ")
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
    fn joins_spelled_letters() {
        assert_eq!(letters("Show the dashboard on the T V."), "Show the dashboard on the TV.");
        assert_eq!(letters("Turn off the T V"), "Turn off the TV");
        assert_eq!(letters("I want a B"), "I want a B");
        assert_eq!(letters("Play B B C News"), "Play BBC News");
    }

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
