//! Norwegian words in an English assistant: OpenAI Whisper (int8, through sherpa-onnx) told the
//! language is Norwegian, for what the English listener cannot spell, such as shopping list items
//! said by their Norwegian names ("brunost", "rømme"). Called only for those, so its cost is paid
//! only then.

use std::path::PathBuf;

use anyhow::{Context, Result};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineWhisperModelConfig};

use crate::whisper::resample_24k_to_16k;

/// sherpa-onnx's export of whisper-small: the smallest that writes Norwegian groceries well
/// (docs/benchmarks.md).
pub const DEFAULT_REPO: &str = "csukuangfj/sherpa-onnx-whisper-small";

pub struct NorwegianFiles {
    encoder: PathBuf,
    decoder: PathBuf,
    tokens: PathBuf,
}

impl NorwegianFiles {
    /// `repo` is `csukuangfj/sherpa-onnx-whisper-<size>`; its files are named after the size.
    pub fn fetch(repo: &str) -> Result<Self> {
        let size = repo.rsplit('-').next().unwrap_or("small");
        let api = hf_hub::api::sync::ApiBuilder::from_env().build()?;
        let repo_api = api.model(repo.to_owned());
        let get = |file: String| repo_api.get(&file).with_context(|| format!("fetching {repo}/{file}"));
        Ok(Self {
            encoder: get(format!("{size}-encoder.int8.onnx"))?,
            decoder: get(format!("{size}-decoder.int8.onnx"))?,
            tokens: get(format!("{size}-tokens.txt"))?,
        })
    }

    pub fn encoder(&self) -> &PathBuf {
        &self.encoder
    }
}

pub struct Norwegian {
    recognizer: OfflineRecognizer,
}

impl Norwegian {
    pub fn load(files: &NorwegianFiles, threads: usize) -> Result<Self> {
        let path = |p: &PathBuf| Some(p.to_string_lossy().into_owned());
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.whisper = OfflineWhisperModelConfig {
            encoder: path(&files.encoder),
            decoder: path(&files.decoder),
            language: Some("no".into()),
            task: Some("transcribe".into()),
            ..Default::default()
        };
        config.model_config.tokens = path(&files.tokens);
        config.model_config.num_threads = threads.max(1) as i32;
        let recognizer = OfflineRecognizer::create(&config).context("loading Whisper for Norwegian")?;
        Ok(Self { recognizer })
    }

    /// The Norwegian text of 24 kHz speech; empty without words.
    pub fn transcribe(&self, pcm: &[i16]) -> String {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(16_000, &resample_24k_to_16k(pcm));
        self.recognizer.decode(&stream);
        stream.get_result().map(|r| r.text.trim().to_owned()).unwrap_or_default()
    }
}
