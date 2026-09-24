//! English speech with Kyutai Pocket TTS (100M parameters, CPU, streaming) through `ptts`, the
//! `xn`-based port. Weights and the preset voices come from the ungated
//! `kyutai/pocket-tts-without-voice-cloning` repository.

use std::path::Path as FsPath;

use anyhow::{bail, Context, Result};
use ptts::tts_model::{prepare_text_prompt, split_into_best_sentences, TTSConfig, TTSModel, TTSState};
use xn::nn::VB;
use xn::{BackendQ, Tensor};

use crate::lang::Lang;
use crate::tts::Tts;

const HF_REPO: &str = "kyutai/pocket-tts-without-voice-cloning";
const MODEL_FILE: &str = "tts_b6369a24.safetensors";
/// Preset voices that exist as single-tensor embeddings in the repository.
pub const VOICES: &[&str] = &["alba", "marius", "javert", "jean", "fantine", "cosette", "eponine", "azelma"];
/// Kyutai's default sampling temperature.
const TEMPERATURE: f32 = 0.7;
/// Flow-LM sequence budget: voice prompt (125 frames) plus one sentence of text and audio.
const SEQ_BUDGET: usize = 1024;
/// Mimi decoder context in frames.
const MIMI_CONTEXT: usize = 250;

pub struct PocketFiles {
    pub model: std::path::PathBuf,
    pub tokenizer: std::path::PathBuf,
    pub voice: std::path::PathBuf,
}

impl PocketFiles {
    /// About 240 MB on first use.
    pub fn fetch(voice: &str) -> Result<Self> {
        if !VOICES.contains(&voice) {
            bail!("unknown voice {voice}; available: {}", VOICES.join(", "));
        }
        let api = hf_hub::api::sync::ApiBuilder::from_env().build()?;
        let repo = api.model(HF_REPO.to_owned());
        let get = |file: &str| repo.get(file).with_context(|| format!("fetching {HF_REPO}/{file}"));
        Ok(Self {
            model: get(MODEL_FILE)?,
            tokenizer: get("tokenizer.json")?,
            voice: get(&format!("embeddings/{voice}.safetensors"))?,
        })
    }
}

/// Sizes xn's compute pool (separate from candle's rayon pool). Set it to the pod's CPU count.
pub fn set_threads(threads: usize) {
    xn::set_num_threads(threads.max(1));
}

/// Weights for the CPU: plain f32, or 8-bit linear layers (smaller, faster on weak CPUs).
#[derive(Clone, Copy, Debug)]
pub enum Precision {
    F32,
    Q8,
}

/// Loads Pocket TTS on the CPU.
pub fn load(files: &PocketFiles, precision: Precision) -> Result<Box<dyn Tts>> {
    Ok(match precision {
        Precision::F32 => Box::new(Pocket::<xn::Unquantized<f32, xn::CpuDevice>>::load(files)?),
        Precision::Q8 => Box::new(Pocket::<xn::quantized::Q80F32>::load(files)?),
    })
}

struct HfTokenizer(tokenizers::Tokenizer);

impl ptts::Tokenizer for HfTokenizer {
    fn encode(&self, text: &str) -> xn::Result<Vec<u32>> {
        let encoding = self.0.encode(text, false).map_err(|e| xn::Error::Msg(e.to_string()))?;
        Ok(encoding.get_ids().to_vec())
    }

    fn decode(&self, tokens: &[u32]) -> xn::Result<String> {
        self.0.decode(tokens, false).map_err(|e| xn::Error::Msg(e.to_string()))
    }
}

struct Normal {
    rng: rand::rngs::StdRng,
    distr: rand_distr::Normal<f32>,
}

impl ptts::flow_lm::Rng for Normal {
    fn sample(&mut self) -> f32 {
        use rand::Rng;
        self.rng.sample(self.distr)
    }
}

struct Pocket<Q: BackendQ<B = xn::CpuDevice>> {
    model: TTSModel<Q>,
    /// Flow-LM state after the voice prompt; cloned for every sentence.
    voiced: TTSState<Q>,
    tokenizer: HfTokenizer,
    rng: Normal,
    ldim: usize,
}

/// Checkpoint names -> the names `ptts` modules expect (from ptts's own examples).
fn remap_key(name: &str) -> Option<String> {
    if name.contains("flow.w_s_t") || name.contains("quantizer.vq") || name.contains("quantizer.logvar_proj") {
        return None;
    }
    Some(
        name.replace(
            "flow_lm.condition_provider.conditioners.speaker_wavs.output_proj.weight",
            "flow_lm.speaker_proj_weight",
        )
        .replace("flow_lm.condition_provider.conditioners.transcript_in_segment.", "flow_lm.conditioner.")
        .replace("flow_lm.backbone.", "flow_lm.transformer.")
        .replace("flow_lm.flow.", "flow_lm.flow_net.")
        .replace("mimi.model.", "mimi."),
    )
}

impl<Q: BackendQ<B = xn::CpuDevice>> Pocket<Q> {
    fn load(files: &PocketFiles) -> Result<Self> {
        use rand::SeedableRng;
        let cfg = TTSConfig::v202601(TEMPERATURE);
        let vb = VB::load_with_key_map(&[&files.model], xn::CPU, remap_key)?.root();
        let hf = tokenizers::Tokenizer::from_file(&files.tokenizer)
            .map_err(|e| anyhow::anyhow!("loading {}: {e}", files.tokenizer.display()))?;
        let model: TTSModel<Q> = TTSModel::load(&vb, Box::new(HfTokenizer(hf.clone())), &cfg)?;
        let mut voiced = model.init_flow_lm_state(1, SEQ_BUDGET)?;
        model.prompt_audio(&mut voiced, &voice_embedding::<Q>(&files.voice)?)?;
        let rng = Normal {
            rng: rand::rngs::StdRng::seed_from_u64(0x5eed),
            distr: rand_distr::Normal::new(0.0, TEMPERATURE.sqrt())?,
        };
        Ok(Self { model, voiced, tokenizer: HfTokenizer(hf), rng, ldim: cfg.flow_lm.ldim })
    }

    fn sentence(&mut self, text: &str, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        let (text, frames_after_eos) = prepare_text_prompt(text);
        let tokens = self.model.flow_lm.conditioner.tokenize(&text)?;
        // Same cut-off as ptts's example: about three tokens per 80 ms frame, plus slack.
        let max_frames = ((tokens.len() as f64 / 3.0 + 2.0) * 12.5).ceil() as usize;
        let mut state = self.voiced.clone();
        let mut mimi = self.model.init_mimi_state(1, MIMI_CONTEXT)?;
        self.model.prompt_text(&mut state, &tokens)?;
        let mut prev: Tensor<Q::T, xn::CpuDevice> =
            Tensor::from_vec(vec![f32::NAN; self.ldim], (1, 1, self.ldim), &xn::CPU)?.to::<Q::T>()?;
        let mut countdown: Option<usize> = None;
        let mut pcm = Vec::new();
        for _ in 0..max_frames {
            let (latent, eos) = self.model.generate_step(&mut state, &prev, &mut self.rng)?;
            let audio: Vec<f32> = self.model.decode_latent(&latent.to()?, &mut mimi)?.to_vec()?;
            pcm.clear();
            pcm.extend(audio.iter().map(|s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16));
            sink(&pcm);
            if eos && countdown.is_none() {
                countdown = Some(frames_after_eos);
            }
            match &mut countdown {
                Some(0) => break,
                Some(n) => *n -= 1,
                None => {}
            }
            prev = latent;
        }
        Ok(())
    }
}

fn voice_embedding<Q: BackendQ<B = xn::CpuDevice>>(path: &FsPath) -> Result<Tensor<Q::T, xn::CpuDevice>> {
    let vb = VB::load(&[path], xn::CPU)?;
    let names = vb.tensor_names();
    let key = names.first().context("no tensor in voice embedding")?;
    let shape = vb.shape(key).context("voice tensor has no shape")?;
    let dims = shape.dims().to_vec();
    let emb: Tensor<f32, xn::CpuDevice> = vb.tensor(key, shape)?;
    let emb = if dims.len() == 2 { emb.reshape((1, dims[0], dims[1]))? } else { emb };
    Ok(emb.to::<Q::T>()?)
}

impl<Q: BackendQ<B = xn::CpuDevice>> Tts for Pocket<Q>
where
    TTSModel<Q>: Send,
    TTSState<Q>: Send,
{
    fn speak(&mut self, text: &str, _lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        for chunk in split_into_best_sentences(&self.tokenizer, text, None)? {
            self.sentence(&chunk, sink)?;
        }
        Ok(())
    }
}
