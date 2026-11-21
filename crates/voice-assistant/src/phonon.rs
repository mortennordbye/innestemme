//! Phonon 31M (Gradium, Apache 2.0) in the engine through its Rust runtime `ptts`: English speech
//! streamed as it is made, so the first sound comes in tens of milliseconds where Kokoro renders a
//! whole sentence first. It says numbers itself; sherpa-onnx 2's Kokoro dropped them.
//!
//! The model and the voice are fetched into the Hugging Face cache, each pinned to one commit, so a
//! restart never changes how the engine sounds (the speech cache keeps what it rendered).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use ptts::checkpoint::{Checkpoint, ResolveOptions};
use ptts::synth::{DeviceKind, Quant, Synth};

use crate::lang::Lang;
use crate::models::HubRepo;
use crate::resample::to_i16;
use crate::tts::Tts;

/// The English model, with its tokenizer and config.
pub const MODEL_REPO: &str = "gradium/phonon-31m-en";
pub const MODEL_REVISION: &str = "92741c5b07d085ff840e26fea8ce43110b87852f";
/// Gradium's voice catalogue: `voices.jsonl` names each `voices/<id>.safetensors`.
pub const VOICES_REPO: &str = "gradium/phonon-voices";
pub const VOICES_REVISION: &str = "0ee17205f254b5abc43b266c8a0f215565c352f7";

/// Sizes Phonon's own thread pool. It reads `RAYON_NUM_THREADS` otherwise, or counts the host's
/// cores, not the pod's. Sets an environment variable, so call it before other threads start.
pub fn set_threads(threads: usize) {
    xn::set_num_threads(threads.max(1));
}

/// The model folder and the voice's embedding, in the Hugging Face cache.
pub struct PhononFiles {
    model: PathBuf,
    voice: PathBuf,
}

impl PhononFiles {
    /// `int8`: the q8 weights, faster on the cluster's CPU (4.5x against 3.2x real time on 4
    /// threads) and loaded in half the time.
    pub fn fetch(voice: &str, int8: bool) -> Result<Self> {
        let model = HubRepo::at(MODEL_REPO, Some(MODEL_REVISION))?;
        let weights = if int8 { "model.q8.gguf" } else { "model.safetensors" };
        let mut config = None;
        for file in ["config.json", "tokenizer.json", weights] {
            let path = model.get(file)?;
            if file == "config.json" {
                config = Some(path);
            }
        }
        let model = config.context("no config.json")?.parent().context("config.json without a folder")?.to_owned();
        let voices = HubRepo::at(VOICES_REPO, Some(VOICES_REVISION))?;
        let catalogue = voices.get("voices.jsonl")?;
        let file = voice_file(&std::fs::read_to_string(&catalogue)?, voice)?;
        let voice = voices.get(&file)?;
        Ok(Self { model, voice })
    }

    pub fn model(&self) -> &std::path::Path {
        &self.model
    }
}

/// The embedding file of an English voice in the catalogue, by name ("Hugh"), any letter case.
fn voice_file(catalogue: &str, voice: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        filename: String,
        language: String,
    }
    let entries: Vec<Entry> =
        catalogue.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();
    if let Some(entry) = entries.iter().find(|e| e.language == "en" && e.name.eq_ignore_ascii_case(voice)) {
        return Ok(entry.filename.clone());
    }
    let english: Vec<&str> = entries.iter().filter(|e| e.language == "en").map(|e| e.name.as_str()).collect();
    bail!("Phonon has no English voice {voice}; one of {}", english.join(", "))
}

pub struct Phonon {
    synth: Synth,
}

impl Phonon {
    /// `temperature` 0 is the same take every time: at the default 0.3 a number at the end of a
    /// sentence sometimes gets a sound after it ("The high today is 7-Bone").
    pub fn load(files: &PhononFiles, voice: &str, temperature: f32, int8: bool) -> Result<Self> {
        let quant = if int8 { Quant::Q80 } else { Quant::F32 };
        let checkpoint = Checkpoint::resolve(&files.model, ResolveOptions { quant, weights: None })
            .context("opening the Phonon checkpoint")?;
        let synth = checkpoint
            .builder(ptts::preprocess::Lang::En)
            .device(DeviceKind::Cpu)
            .add_voice(voice, &files.voice)
            .voice(voice)
            .temperature(temperature)
            .build()
            .context("loading Phonon")?;
        anyhow::ensure!(synth.sample_rate() == 24_000, "Phonon speaks at {} Hz, not 24 kHz", synth.sample_rate());
        let phonon = Self { synth };
        // The first call sets up buffers; it is not what a sentence costs.
        phonon.synth.say("Hello.").context("Phonon's first sentence")?;
        Ok(phonon)
    }
}

impl Tts for Phonon {
    fn speak(&mut self, text: &str, _lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        let mut spoken = false;
        for chunk in self.synth.stream(text).context("Phonon")? {
            let pcm: Vec<i16> = chunk?.iter().copied().map(to_i16).collect();
            if !pcm.is_empty() {
                spoken = true;
                sink(&pcm);
            }
        }
        if !spoken {
            bail!("Phonon made no audio");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOGUE: &str = r#"{"filename": "voices/YyKcxPG7iIgS3jsK.safetensors", "name": "Hugh", "voice_id": "YyKcxPG7iIgS3jsK", "language": "en"}
{"filename": "voices/abc.safetensors", "name": "Anton", "voice_id": "abc", "language": "de"}
"#;

    #[test]
    fn finds_an_english_voice_by_name() {
        assert_eq!(voice_file(CATALOGUE, "hugh").unwrap(), "voices/YyKcxPG7iIgS3jsK.safetensors");
        let error = voice_file(CATALOGUE, "Anton").unwrap_err().to_string();
        assert!(error.contains("no English voice Anton") && error.contains("Hugh"), "{error}");
    }
}
