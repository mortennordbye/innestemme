//! Whether a voice sounds male or female, so the persona can address the speaker without knowing
//! who it is. A speaker embedding (3D-Speaker's CAM++ trained on VoxCeleb, through sherpa-onnx) and
//! a logistic regression on it, trained on LibriSpeech by `examples/voice_gender_train.rs`.
//!
//! It goes by how a voice sounds, so children and some adults come out wrong: answers stay neutral
//! unless the classifier is sure.

use anyhow::{anyhow, Context, Result};
use sherpa_onnx::{SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig};

use crate::wakeword::{cache_dir, fetch_from};

const RELEASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models";
/// (file, SHA-256); Apache 2.0.
const MODEL: (&str, &str) = (
    "3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx",
    "357a834f702b80161e5b981182c038e18553c1f2ca752ed6cec2052365d4129b",
);
pub const SAMPLE_RATE: u32 = 16_000;
/// Below this a voice is not sure enough either way and gets no honorific. A wrong "sir" or
/// "ma'am" is noticed; a missing one is not.
pub const SURE: f32 = 0.85;
/// Shorter audio gives an embedding that says little.
const SHORTEST: usize = SAMPLE_RATE as usize / 2;

include!("voice_gender_weights.rs");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gender {
    Male,
    Female,
}

pub struct VoiceGender {
    extractor: SpeakerEmbeddingExtractor,
}

impl VoiceGender {
    /// Downloads the model to `$HF_HOME/speaker` on first use.
    pub fn load(threads: usize) -> Result<Self> {
        let model = fetch_from(&cache_dir().join("speaker"), RELEASE, MODEL)?;
        let config = SpeakerEmbeddingExtractorConfig {
            model: Some(model.to_string_lossy().into_owned()),
            num_threads: threads.max(1) as i32,
            ..Default::default()
        };
        let extractor = SpeakerEmbeddingExtractor::create(&config).context("loading the speaker embedding model")?;
        anyhow::ensure!(extractor.dim() as usize == WEIGHTS.len(), "speaker embedding of {} values", extractor.dim());
        Ok(Self { extractor })
    }

    /// The voice's embedding, of unit length; `pcm` is 16 kHz mono.
    pub fn embedding(&self, pcm: &[i16]) -> Result<Vec<f32>> {
        anyhow::ensure!(pcm.len() >= SHORTEST, "too little audio");
        let stream = self.extractor.create_stream().ok_or_else(|| anyhow!("no stream"))?;
        let samples: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        stream.accept_waveform(SAMPLE_RATE as i32, &samples);
        stream.input_finished();
        anyhow::ensure!(self.extractor.is_ready(&stream), "too little audio");
        let mut embedding = self.extractor.compute(&stream).ok_or_else(|| anyhow!("no embedding"))?;
        let norm = embedding.iter().map(|v| v * v).sum::<f32>().sqrt().max(f32::EPSILON);
        embedding.iter_mut().for_each(|v| *v /= norm);
        Ok(embedding)
    }

    /// How likely the voice in `pcm` (16 kHz) is female, 0 to 1.
    pub fn female(&self, pcm: &[i16]) -> Result<f32> {
        Ok(female(&self.embedding(pcm)?))
    }

    /// The voice's gender when the classifier is sure of it.
    pub fn classify(&self, pcm: &[i16]) -> Result<Option<Gender>> {
        Ok(sure(self.female(pcm)?))
    }
}

/// The probability of female from an embedding of unit length.
pub fn female(embedding: &[f32]) -> f32 {
    let z: f32 = BIAS + embedding.iter().zip(WEIGHTS).map(|(x, w)| x * w).sum::<f32>();
    1.0 / (1.0 + (-z).exp())
}

pub fn sure(female: f32) -> Option<Gender> {
    if female >= SURE {
        Some(Gender::Female)
    } else if female <= 1.0 - SURE {
        Some(Gender::Male)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsure_in_the_middle() {
        assert_eq!(sure(0.5), None);
        assert_eq!(sure(0.97), Some(Gender::Female));
        assert_eq!(sure(0.02), Some(Gender::Male));
    }
}
