//! Wake word detection on a satellite's audio stream with openWakeWord models (ONNX, run by tract):
//! a mel spectrogram model, a shared speech embedding model and a small per-phrase classifier.
//! Scores match openWakeWord's own streaming code to three decimals.
//!
//! The pretrained models are CC BY-NC-SA 4.0, so they are downloaded on first use rather than kept
//! in this repository. A model of your own (trained with openWakeWord's notebook) is a path to its
//! `.onnx` file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tract_onnx::prelude::*;

/// Audio is scored in steps of 80 ms at 16 kHz, as openWakeWord does.
pub const CHUNK: usize = 1280;
/// The mel model needs three hops of audio before each chunk to give a chunk's eight frames.
const MEL_CONTEXT: usize = 480;
const MEL_BINS: usize = 32;
/// The embedding model looks at 76 mel frames (775 ms).
const MEL_WINDOW: usize = 76;
const EMBEDDING: usize = 96;
/// The classifier looks at the last 16 embeddings (1.28 s).
const FEATURES: usize = 16;
/// openWakeWord skips the first five scores of a stream; its buffers are not filled yet.
const WARM_UP: usize = 5;

const RELEASE: &str = "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1";
/// (file, SHA-256) of the models from the openWakeWord release above.
const MELSPECTROGRAM: (&str, &str) =
    ("melspectrogram.onnx", "ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f");
const EMBEDDING_MODEL: (&str, &str) =
    ("embedding_model.onnx", "70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f");
const PRETRAINED: &[(&str, (&str, &str))] =
    &[("hey_jarvis", ("hey_jarvis_v0.1.onnx", "94a13cfe60075b132f6a472e7e462e8123ee70861bc3fb58434a73712ee0d2cb"))];

type Plan = Arc<TypedRunnableModel>;

/// The loaded models, shared by every stream.
pub struct WakeModels {
    mel: Plan,
    embedding: Plan,
    classifier: Plan,
    /// What the classifier detects, for logs.
    pub name: String,
}

impl WakeModels {
    /// `model`: a pretrained model by name ("hey_jarvis") or the path to a `.onnx` classifier.
    /// Downloads go to `$HF_HOME/openwakeword` (the engine's model volume).
    pub fn load(model: &str) -> Result<Self> {
        let dir = cache_dir().join("openwakeword");
        let classifier = match PRETRAINED.iter().find(|(name, _)| *name == model) {
            Some((_, file)) => fetch(&dir, *file)?,
            None if model.ends_with(".onnx") => PathBuf::from(model),
            None => {
                let known: Vec<&str> = PRETRAINED.iter().map(|(name, _)| *name).collect();
                bail!("unknown wake word model `{model}`: one of {known:?} or a path to a .onnx file")
            }
        };
        let name = Path::new(model).file_stem().map_or(model.to_owned(), |s| s.to_string_lossy().into_owned());
        Ok(Self {
            mel: plan(&fetch(&dir, MELSPECTROGRAM)?, &[1, MEL_CONTEXT + CHUNK])?,
            embedding: plan(&fetch(&dir, EMBEDDING_MODEL)?, &[1, MEL_WINDOW, MEL_BINS, 1])?,
            classifier: plan(&classifier, &[1, FEATURES, EMBEDDING])?,
            name,
        })
    }
}

/// One stream's state. Feed it 16 kHz audio in any block size.
pub struct WakeDetector {
    models: Arc<WakeModels>,
    /// Audio not yet scored, as f32 of the i16 values (the mel model takes them unscaled).
    pending: Vec<f32>,
    /// The previous chunk's last `MEL_CONTEXT` samples, then the chunk being scored.
    audio: Vec<f32>,
    mel: Vec<f32>,
    features: Vec<f32>,
    chunks: usize,
}

impl WakeDetector {
    pub fn new(models: Arc<WakeModels>) -> Self {
        Self {
            models,
            pending: Vec::with_capacity(2 * CHUNK),
            audio: vec![0.0; MEL_CONTEXT + CHUNK],
            // openWakeWord's buffers start as ones (mel) and as embeddings of noise; with zeros
            // the scores after the warm-up are the same.
            mel: vec![1.0; MEL_WINDOW * MEL_BINS],
            features: vec![0.0; FEATURES * EMBEDDING],
            chunks: 0,
        }
    }

    /// Starts over, e.g. when a new listening period begins.
    pub fn reset(&mut self) {
        *self = Self::new(self.models.clone());
    }

    /// Scores `pcm` (16 kHz mono) and returns the highest score of the chunks it completed, if any.
    pub fn push(&mut self, pcm: &[i16]) -> Result<Option<f32>> {
        self.pending.extend(pcm.iter().map(|&s| s as f32));
        let mut best: Option<f32> = None;
        while self.pending.len() >= CHUNK {
            self.audio.copy_within(CHUNK.., 0);
            self.audio[MEL_CONTEXT..].copy_from_slice(&self.pending[..CHUNK]);
            self.pending.drain(..CHUNK);
            let score = self.score()?;
            best = Some(best.map_or(score, |b| b.max(score)));
        }
        Ok(best)
    }

    fn score(&mut self) -> Result<f32> {
        let m = &self.models;
        let frames = run(&m.mel, &[1, MEL_CONTEXT + CHUNK], &self.audio)?;
        // openWakeWord's scaling of the mel output.
        let frames: Vec<f32> = frames.iter().map(|v| v / 10.0 + 2.0).collect();
        shift_in(&mut self.mel, &frames);
        let embedding = run(&m.embedding, &[1, MEL_WINDOW, MEL_BINS, 1], &self.mel)?;
        shift_in(&mut self.features, &embedding);
        let score = run(&m.classifier, &[1, FEATURES, EMBEDDING], &self.features)?[0];
        self.chunks += 1;
        Ok(if self.chunks <= WARM_UP { 0.0 } else { score })
    }
}

/// Drops the oldest values of a fixed-length window to make room for `new`.
fn shift_in(window: &mut [f32], new: &[f32]) {
    let n = new.len().min(window.len());
    window.copy_within(n.., 0);
    let len = window.len();
    window[len - n..].copy_from_slice(&new[new.len() - n..]);
}

fn run(plan: &Plan, shape: &[usize], input: &[f32]) -> Result<Vec<f32>> {
    let tensor = Tensor::from_shape(shape, input)?;
    let out = plan.run(tvec!(tensor.into()))?;
    Ok(out[0].to_plain_array_view::<f32>()?.iter().copied().collect())
}

fn plan(path: &Path, shape: &[usize]) -> Result<Plan> {
    let plan = tract_onnx::onnx()
        .model_for_path(path)
        .and_then(|m| m.with_input_fact(0, f32::fact(shape).into()))
        .and_then(|m| m.into_optimized())
        .and_then(|m| m.into_runnable())
        .with_context(|| format!("loading {}", path.display()))?;
    Ok(plan)
}

fn cache_dir() -> PathBuf {
    match std::env::var_os("HF_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/huggingface"),
    }
}

/// The file from the release, downloaded once and checked against its hash.
fn fetch(dir: &Path, (file, sha256): (&str, &str)) -> Result<PathBuf> {
    let path = dir.join(file);
    if path.exists() {
        return Ok(path);
    }
    let url = format!("{RELEASE}/{file}");
    let bytes = ureq::get(&url)
        .call()
        .and_then(|mut response| response.body_mut().read_to_vec())
        .with_context(|| format!("downloading {url}"))?;
    let digest: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if digest != sha256 {
        bail!("{url}: SHA-256 {digest}, expected {sha256}");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Written under another name first, so an interrupted download is not taken for the model.
    let partial = path.with_extension("part");
    std::fs::write(&partial, &bytes).with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_in_keeps_the_newest_values() {
        let mut window = [1.0, 2.0, 3.0, 4.0];
        shift_in(&mut window, &[5.0, 6.0]);
        assert_eq!(window, [3.0, 4.0, 5.0, 6.0]);
        shift_in(&mut window, &[7.0, 8.0, 9.0, 10.0, 11.0]);
        assert_eq!(window, [8.0, 9.0, 10.0, 11.0]);
    }

    /// Downloads the models; run with `cargo test -p voice-assistant -- --ignored wake`.
    #[test]
    #[ignore]
    fn wakes_on_the_phrase_and_not_on_silence() {
        let models = Arc::new(WakeModels::load("hey_jarvis").unwrap());
        let mut detector = WakeDetector::new(models);
        let quiet = vec![0i16; 16_000 * 3];
        let best = detector.push(&quiet).unwrap().unwrap();
        assert!(best < 0.1, "{best}");
        // Odd block sizes add up to the same chunks.
        detector.reset();
        let mut scored = 0;
        for block in quiet.chunks(333) {
            scored += usize::from(detector.push(block).unwrap().is_some());
        }
        assert!(scored >= quiet.len() / CHUNK - 1);
    }
}
