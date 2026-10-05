//! Whisper's text decoder with a key/value cache for self-attention. candle's decoder caches only
//! the cross-attention and runs every earlier token again on each step, which on a small CPU costs
//! more than the step itself. Same weights and arithmetic as `candle_transformers`' decoder.

use candle::{Result, Tensor, D};
use candle_nn::{Embedding, LayerNorm, Linear, Module, VarBuilder};
use candle_transformers::models::whisper::Config;

struct Attention {
    query: Linear,
    key: Linear,
    value: Linear,
    out: Linear,
    n_head: usize,
    /// Keys and values so far: every earlier token for self-attention, the audio for cross-attention.
    cache: Option<(Tensor, Tensor)>,
}

impl Attention {
    fn load(n_state: usize, n_head: usize, vb: VarBuilder) -> Result<Self> {
        Ok(Self {
            query: candle_nn::linear(n_state, n_state, vb.pp("q_proj"))?,
            key: candle_nn::linear_no_bias(n_state, n_state, vb.pp("k_proj"))?,
            value: candle_nn::linear(n_state, n_state, vb.pp("v_proj"))?,
            out: candle_nn::linear(n_state, n_state, vb.pp("out_proj"))?,
            n_head,
            cache: None,
        })
    }

    /// Self-attention over the cached tokens and `x`, which are appended to the cache.
    fn attend_self(&mut self, x: &Tensor, mask: Option<&Tensor>) -> Result<Tensor> {
        let (k, v) = (self.key.forward(x)?, self.value.forward(x)?);
        let (k, v) = match &self.cache {
            Some((ck, cv)) => (Tensor::cat(&[ck, &k], 1)?, Tensor::cat(&[cv, &v], 1)?),
            None => (k, v),
        };
        self.cache = Some((k.clone(), v.clone()));
        self.attention(x, &k, &v, mask)
    }

    /// Cross-attention over the audio set with `set_audio`.
    fn attend_audio(&self, x: &Tensor) -> Result<Tensor> {
        let (k, v) = self.cache.as_ref().ok_or_else(|| candle::Error::Msg("no audio features".into()))?;
        self.attention(x, k, v, None)
    }

    fn set_audio(&mut self, features: &Tensor) -> Result<()> {
        self.cache = Some((self.key.forward(features)?, self.value.forward(features)?));
        Ok(())
    }

    fn heads(&self, x: &Tensor) -> Result<Tensor> {
        let (batch, len, n_state) = x.dims3()?;
        x.reshape((batch, len, self.n_head, n_state / self.n_head))?.transpose(1, 2)
    }

    fn attention(&self, x: &Tensor, k: &Tensor, v: &Tensor, mask: Option<&Tensor>) -> Result<Tensor> {
        let n_state = x.dim(D::Minus1)?;
        let scale = ((n_state / self.n_head) as f64).powf(-0.25);
        let q = (self.heads(&self.query.forward(x)?)? * scale)?;
        let k = (self.heads(k)?.transpose(2, 3)? * scale)?;
        let v = self.heads(v)?.contiguous()?;
        let mut qk = q.matmul(&k)?;
        if let Some(mask) = mask {
            qk = qk.broadcast_add(mask)?;
        }
        let weights = candle_nn::ops::softmax_last_dim(&qk)?;
        let wv = weights.matmul(&v)?.transpose(1, 2)?.flatten_from(2)?;
        self.out.forward(&wv)
    }
}

struct Block {
    attn: Attention,
    attn_ln: LayerNorm,
    cross: Attention,
    cross_ln: LayerNorm,
    fc1: Linear,
    fc2: Linear,
    mlp_ln: LayerNorm,
}

impl Block {
    fn load(n_state: usize, n_head: usize, vb: VarBuilder) -> Result<Self> {
        Ok(Self {
            attn: Attention::load(n_state, n_head, vb.pp("self_attn"))?,
            attn_ln: candle_nn::layer_norm(n_state, 1e-5, vb.pp("self_attn_layer_norm"))?,
            cross: Attention::load(n_state, n_head, vb.pp("encoder_attn"))?,
            cross_ln: candle_nn::layer_norm(n_state, 1e-5, vb.pp("encoder_attn_layer_norm"))?,
            fc1: candle_nn::linear(n_state, 4 * n_state, vb.pp("fc1"))?,
            fc2: candle_nn::linear(4 * n_state, n_state, vb.pp("fc2"))?,
            mlp_ln: candle_nn::layer_norm(n_state, 1e-5, vb.pp("final_layer_norm"))?,
        })
    }

    fn forward(&mut self, x: &Tensor, mask: Option<&Tensor>) -> Result<Tensor> {
        let x = (x + self.attn.attend_self(&self.attn_ln.forward(x)?, mask)?)?;
        let x = (&x + self.cross.attend_audio(&self.cross_ln.forward(&x)?)?)?;
        let mlp = self.fc2.forward(&self.fc1.forward(&self.mlp_ln.forward(&x)?)?.gelu()?)?;
        x + mlp
    }
}

pub struct Decoder {
    embedding: Embedding,
    positions: Tensor,
    blocks: Vec<Block>,
    ln: LayerNorm,
    /// Tokens in the self-attention cache.
    len: usize,
}

impl Decoder {
    /// `vb` points at `model.decoder`.
    pub fn load(vb: VarBuilder, cfg: &Config) -> Result<Self> {
        let n_state = cfg.d_model;
        let blocks = (0..cfg.decoder_layers)
            .map(|i| Block::load(n_state, cfg.decoder_attention_heads, vb.pp(format!("layers.{i}"))))
            .collect::<Result<_>>()?;
        Ok(Self {
            embedding: candle_nn::embedding(cfg.vocab_size, n_state, vb.pp("embed_tokens"))?,
            positions: vb.get((cfg.max_target_positions, n_state), "embed_positions.weight")?,
            blocks,
            ln: candle_nn::layer_norm(n_state, 1e-5, vb.pp("layer_norm"))?,
            len: 0,
        })
    }

    /// Starts on new audio: `features` from the encoder.
    pub fn set_audio(&mut self, features: &Tensor) -> Result<()> {
        for block in &mut self.blocks {
            block.cross.set_audio(features)?;
        }
        self.restart();
        Ok(())
    }

    /// Forgets the tokens, keeps the audio.
    pub fn restart(&mut self) {
        for block in &mut self.blocks {
            block.attn.cache = None;
        }
        self.len = 0;
    }

    /// Feeds `tokens` after the ones fed since the last restart and returns the logits for the
    /// next position.
    pub fn step(&mut self, tokens: &[u32]) -> Result<Tensor> {
        let device = self.positions.device().clone();
        let count = tokens.len();
        let input = Tensor::new(tokens, &device)?.unsqueeze(0)?;
        let mut x = self.embedding.forward(&input)?.broadcast_add(&self.positions.narrow(0, self.len, count)?)?;
        // New tokens see every cached one and the new ones up to themselves.
        let mask = (count > 1)
            .then(|| {
                let len = self.len;
                let mask: Vec<f32> = (0..count)
                    .flat_map(|i| (0..len + count).map(move |j| if j > len + i { f32::NEG_INFINITY } else { 0.0 }))
                    .collect();
                Tensor::from_vec(mask, (count, len + count), &device)
            })
            .transpose()?;
        for block in &mut self.blocks {
            x = block.forward(&x, mask.as_ref())?;
        }
        self.len += count;
        let last = self.ln.forward(&x.narrow(1, count - 1, 1)?)?;
        last.matmul(&self.embedding.embeddings().t()?.unsqueeze(0)?)?.squeeze(0)?.squeeze(0)
    }
}
