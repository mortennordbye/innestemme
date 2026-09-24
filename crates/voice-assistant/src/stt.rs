//! Streaming speech-to-text with Kyutai STT-1B (English and French) on candle. One call per 80 ms
//! frame; words come out about 0.5 s after they are spoken.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use candle::{Device, Tensor};
use moshi::asr::{AsrMsg, State};
use voice_proto::FRAME_SAMPLES;

use crate::spm::Pieces;

const HF_REPO: &str = "kyutai/stt-1b-en_fr-candle";
/// Mimi steps per second.
const STEP_RATE: f64 = 12.5;

/// The Apple GPU when asked for and compiled in (`metal` feature), else the CPU.
pub fn device(gpu: bool) -> Result<Device> {
    if !gpu {
        return Ok(Device::Cpu);
    }
    if !candle::utils::metal_is_available() {
        bail!("no GPU backend: build with `--features metal` on a Mac");
    }
    Ok(Device::new_metal(0)?)
}

pub struct SttFiles {
    pub config: PathBuf,
    pub model: PathBuf,
    pub tokenizer: PathBuf,
}

impl SttFiles {
    /// Downloads the weights into the Hugging Face cache (or reuses them), about 2 GB.
    pub fn fetch() -> Result<Self> {
        let api = hf_hub::api::sync::ApiBuilder::from_env().build()?;
        let repo = api.model(HF_REPO.to_owned());
        let get = |file: &str| repo.get(file).with_context(|| format!("fetching {HF_REPO}/{file}"));
        let config = get("config.json")?;
        let tokenizer_name = read_config(&config)?.tokenizer_name;
        Ok(Self { config, model: get("model.safetensors")?, tokenizer: get(&tokenizer_name)? })
    }
}

#[derive(serde::Deserialize)]
struct SttConfig {
    audio_delay_seconds: f64,
}

#[derive(serde::Deserialize)]
struct Config {
    tokenizer_name: String,
    card: usize,
    text_card: usize,
    dim: usize,
    n_q: usize,
    context: usize,
    max_period: f64,
    num_heads: usize,
    num_layers: usize,
    causal: bool,
    extra_heads_num_heads: Option<usize>,
    extra_heads_dim: Option<usize>,
    stt_config: SttConfig,
}

fn read_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

impl Config {
    fn lm(&self) -> moshi::lm::Config {
        let transformer = moshi::transformer::Config {
            d_model: self.dim,
            num_heads: self.num_heads,
            num_layers: self.num_layers,
            dim_feedforward: self.dim * 4,
            causal: self.causal,
            norm_first: true,
            bias_ff: false,
            bias_attn: false,
            layer_scale: None,
            context: self.context,
            max_period: self.max_period as usize,
            use_conv_block: false,
            use_conv_bias: true,
            cross_attention: None,
            gating: Some(candle_nn::Activation::Silu),
            norm: moshi::NormType::RmsNorm,
            positional_embedding: moshi::transformer::PositionalEmbedding::Rope,
            conv_layout: false,
            conv_kernel_size: 3,
            kv_repeat: 1,
            max_seq_len: 4096 * 4,
            shared_cross_attn: false,
        };
        let extra_heads = match (self.extra_heads_num_heads, self.extra_heads_dim) {
            (Some(num_heads), Some(dim)) => Some(moshi::lm::ExtraHeadsConfig { num_heads, dim }),
            _ => None,
        };
        moshi::lm::Config {
            transformer,
            depformer: None,
            audio_vocab_size: self.card + 1,
            text_in_vocab_size: self.text_card + 1,
            text_out_vocab_size: self.text_card,
            audio_codebooks: self.n_q,
            conditioners: Default::default(),
            extra_heads,
        }
    }
}

/// What one frame produced.
#[derive(Debug, Default)]
pub struct SttFrame {
    pub words: Vec<String>,
    /// Probability that nobody speaks for the next 2 s, when the model has pause heads.
    pub pause: Option<f32>,
}

pub struct SpeechToText {
    state: State,
    pieces: Pieces,
    device: Device,
}

impl SpeechToText {
    /// `mimi` is the candle-format Mimi checkpoint the engine already uses; STT reads all 32 of
    /// its codebooks.
    pub fn load(files: &SttFiles, mimi: &Path, device: Device) -> Result<Self> {
        let config = read_config(&files.config)?;
        let pieces = Pieces::load(&files.tokenizer)?;
        // SAFETY: the weights file is not modified while it is mapped.
        let vb = unsafe {
            candle_nn::VarBuilder::from_mmaped_safetensors(&[&files.model], device.bf16_default_to_f32(), &device)?
        };
        let lm = moshi::lm::LmModel::new(&config.lm(), moshi::nn::MaybeQuantizedVarBuilder::Real(vb))
            .with_context(|| format!("loading {}", files.model.display()))?;
        let mimi_path = mimi.to_str().context("mimi path is not valid utf-8")?;
        let audio_tokenizer = moshi::mimi::load(mimi_path, Some(config.n_q), &device)?;
        let delay = (config.stt_config.audio_delay_seconds * STEP_RATE) as usize;
        let state = State::new(1, delay, 0.0, audio_tokenizer, lm)?;
        Ok(Self { state, pieces, device })
    }

    pub fn reset(&mut self) -> Result<()> {
        Ok(self.state.reset()?)
    }

    /// Model steps since the last reset.
    pub fn steps(&self) -> usize {
        self.state.model_step_idx()
    }

    pub fn step(&mut self, pcm: &[i16]) -> Result<SttFrame> {
        if pcm.len() != FRAME_SAMPLES {
            bail!("stt frame must be {FRAME_SAMPLES} samples, got {}", pcm.len());
        }
        let samples: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let input = Tensor::from_vec(samples, (1, 1, FRAME_SAMPLES), &self.device)?;
        let mut frame = SttFrame::default();
        for msg in self.state.step_pcm(input, None, &().into(), |_, _, _| ())? {
            match msg {
                AsrMsg::Word { tokens, .. } => frame.words.push(self.pieces.decode(&tokens)),
                // Heads cover pauses of 0.5, 1, 2 and 3 s; 2 s is Kyutai's end-of-turn signal.
                AsrMsg::Step { prs, .. } => frame.pause = prs.get(2).and_then(|p| p.first().copied()),
                AsrMsg::EndWord { .. } => {}
            }
        }
        Ok(frame)
    }
}
