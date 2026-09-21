use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use candle::{Device, Tensor};
use moshi::mimi::Mimi;
use moshi::{StreamMask, StreamTensor};
use voice_proto::FRAME_SAMPLES;

/// Codebooks per frame. Moshi-family models consume and emit the first 8 of Mimi's 32.
pub const CODEBOOKS: usize = 8;

/// Candle-format Mimi weights. `kyutai/mimi` holds the transformers layout, which candle's loader
/// cannot read.
const HF_REPO: &str = "kyutai/moshiko-candle-q8";
const HF_FILE: &str = "tokenizer-e351c8d8-checkpoint125.safetensors";

/// Streaming Mimi encoder/decoder on CPU: 1920 samples (80 ms at 24 kHz) <-> 8 codes.
///
/// candle allocates a tensor per op, so neither direction is allocation-free and callers inside
/// a `no_alloc` section must wrap these calls in `permit_alloc`. The input is handed over with
/// `Tensor::from_vec` rather than written in place into a reused tensor: streaming convolutions
/// may keep views of their input as carry-over state, and overwriting that storage on the next
/// frame would corrupt them.
pub struct MimiCodec {
    mimi: Mimi,
    device: Device,
    mask: StreamMask,
    pcm_out: Vec<f32>,
}

impl MimiCodec {
    pub fn load(model_file: &Path) -> Result<Self> {
        let device = Device::Cpu;
        let path = model_file.to_str().context("mimi model path is not valid utf-8")?;
        let mimi = moshi::mimi::load(path, Some(CODEBOOKS), &device)
            .with_context(|| format!("loading mimi weights from {path}"))?;
        Ok(Self { mimi, device, mask: StreamMask::empty(), pcm_out: Vec::with_capacity(FRAME_SAMPLES) })
    }

    /// Downloads the weights into the Hugging Face cache (or reuses them) and returns the path.
    pub fn fetch_weights() -> Result<PathBuf> {
        // `Api::new()` ignores HF_HOME; the container image relies on it to reach its volume.
        let api = hf_hub::api::sync::ApiBuilder::from_env().build()?;
        api.model(HF_REPO.to_owned()).get(HF_FILE).with_context(|| format!("fetching {HF_REPO}/{HF_FILE}"))
    }

    pub fn reset(&mut self) {
        self.mimi.reset_state();
    }

    /// Encodes one frame. `None` while the streaming encoder is still priming.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Option<[u32; CODEBOOKS]>> {
        if pcm.len() != FRAME_SAMPLES {
            bail!("mimi frame must be {FRAME_SAMPLES} samples, got {}", pcm.len());
        }
        let samples: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let input = Tensor::from_vec(samples, (1, 1, FRAME_SAMPLES), &self.device)?;
        let codes = self.mimi.encode_step(&StreamTensor::from_tensor(input), &self.mask)?;
        let Some(codes) = codes.as_option() else { return Ok(None) };
        let codes = codes.flatten_all()?.to_vec1::<u32>()?;
        match <[u32; CODEBOOKS]>::try_from(codes.as_slice()) {
            Ok(codes) => Ok(Some(codes)),
            Err(_) => bail!("mimi produced {} codes for one frame, expected {CODEBOOKS}", codes.len()),
        }
    }

    /// Decodes one frame of codes. `None` while the streaming decoder is still priming.
    pub fn decode(&mut self, codes: &[u32; CODEBOOKS]) -> Result<Option<&[f32]>> {
        let codes = Tensor::from_slice(codes, (1, CODEBOOKS, 1), &self.device)?;
        let pcm = self.mimi.decode_step(&StreamTensor::from_tensor(codes), &self.mask)?;
        let Some(pcm) = pcm.as_option() else { return Ok(None) };
        self.pcm_out.clear();
        self.pcm_out.extend(pcm.flatten_all()?.to_vec1::<f32>()?);
        Ok(Some(&self.pcm_out))
    }
}
