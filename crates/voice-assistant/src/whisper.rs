//! Utterance transcription with NB-Whisper (National Library of Norway's Whisper fine-tune:
//! Norwegian and English), greedy decoding, English or Norwegian (Bokmål).
//! Not streaming: one call per finished utterance.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use candle::{Device, IndexOp, Tensor, D};
use candle_transformers::models::whisper::{self as w, audio, model::Whisper};
use tracing::debug;

use crate::lang::Lang;

/// English only: OpenAI's multilingual checkpoints, forced to English. With Norwegian: the
/// National Library of Norway's fine-tune, which transcribes Norwegian far better.
pub fn default_repo(norwegian: bool, size: &str) -> String {
    if norwegian {
        format!("NbAiLab/nb-whisper-{size}")
    } else {
        format!("openai/whisper-{size}")
    }
}
pub const LANGUAGE_ID_REPO: &str = "openai/whisper-tiny";
const MAX_TOKENS: usize = 120;
/// Previous-text context that teaches the decoder how to spell the assistant's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakePrompt {
    None,
    /// " <Name>,": right for NB-Whisper. Stock OpenAI checkpoints read it as the name already said
    /// and leave it out of the transcript.
    Name,
    /// " <Name> is my voice assistant.": names the word without ending on it.
    Context,
}

impl WakePrompt {
    fn text(self, name: &str) -> Option<String> {
        match self {
            Self::None => None,
            Self::Name => Some(format!(" {name},")),
            Self::Context => Some(format!(" {name} is my voice assistant.")),
        }
    }
}
/// Above this, the utterance is treated as noise.
const NO_SPEECH_THRESHOLD: f32 = 0.6;
/// Language-ID probability of English above which an utterance is English.
const ENGLISH_MIN: f32 = 0.5;
/// ... and at least this many times the Nordic languages together.
const ENGLISH_OVER_NORDIC: f32 = 30.0;

pub struct WhisperFiles {
    pub config: PathBuf,
    pub model: PathBuf,
    pub tokenizer: PathBuf,
}

impl WhisperFiles {
    pub fn fetch(repo: &str) -> Result<Self> {
        let api = hf_hub::api::sync::ApiBuilder::from_env().build()?;
        let repo_api = api.model(repo.to_owned());
        let get = |file: &str| repo_api.get(file).with_context(|| format!("fetching {repo}/{file}"));
        Ok(Self { config: get("config.json")?, model: get("model.safetensors")?, tokenizer: get("tokenizer.json")? })
    }
}

pub struct Transcript {
    pub lang: Lang,
    pub text: String,
    /// Mean token log probability.
    pub score: f32,
}

/// One loaded Whisper checkpoint and its special tokens.
struct Model {
    whisper: Whisper,
    vocab: Vocab,
    filters: Vec<f32>,
    device: Device,
    sot: u32,
    eot: u32,
    no_speech: Option<u32>,
    langs: Vec<(Lang, u32)>,
    english: u32,
    /// Norwegian, Nynorsk, Danish, Swedish: where small models put Norwegian speech.
    nordic: Vec<u32>,
}

impl Model {
    fn load(files: &WhisperFiles, device: &Device) -> Result<Self> {
        let config: w::Config = serde_json::from_str(&std::fs::read_to_string(&files.config)?)?;
        let vocab = Vocab::load(&files.tokenizer)?;
        // SAFETY: the weights file is not modified while it is mapped.
        let vb = unsafe { candle_nn::VarBuilder::from_mmaped_safetensors(&[&files.model], w::DTYPE, device)? };
        let filters = mel_filters(config.num_mel_bins);
        let id = |t: &str| vocab.special(t).with_context(|| format!("token {t} missing"));
        let (sot, eot) = (id(w::SOT_TOKEN)?, id(w::EOT_TOKEN)?);
        let no_speech = w::NO_SPEECH_TOKENS.iter().find_map(|t| vocab.special(t));
        let langs = vec![(Lang::English, id("<|en|>")?), (Lang::Norwegian, id("<|no|>")?)];
        let english = id("<|en|>")?;
        let nordic = ["<|no|>", "<|nn|>", "<|da|>", "<|sv|>"].iter().map(|t| id(t)).collect::<Result<_>>()?;
        let whisper = Whisper::load(&vb, config)?;
        Ok(Self { whisper, vocab, filters, device: device.clone(), sot, eot, no_speech, langs, english, nordic })
    }

    fn features(&mut self, samples: &[f32]) -> Result<Tensor> {
        let bins = self.whisper.config.num_mel_bins;
        let mel = audio::pcm_to_mel(&self.whisper.config, samples, &self.filters);
        let frames = mel.len() / bins;
        let mel = Tensor::from_vec(mel, (1, bins, frames), &self.device)?.narrow(2, 0, w::N_FRAMES.min(frames))?;
        Ok(self.whisper.encoder.forward(&mel, true)?)
    }

    /// Logits for the position after `tokens`.
    fn logits(&mut self, tokens: &[u32], features: &Tensor, flush: bool) -> Result<Tensor> {
        let input = Tensor::new(tokens, &self.device)?.unsqueeze(0)?;
        let ys = self.whisper.decoder.forward(&input, features, flush)?;
        let last = ys.i((.., tokens.len() - 1..))?;
        Ok(self.whisper.decoder.final_linear(&last)?.i((0, 0))?)
    }

    /// Probabilities of "no speech", English and the Nordic languages, from the first decoder step.
    fn first_step(&mut self, features: &Tensor) -> Result<(f32, f32, f32)> {
        let probs = candle_nn::ops::softmax(&self.logits(&[self.sot], features, true)?, D::Minus1)?.to_vec1::<f32>()?;
        let nordic = self.nordic.iter().map(|&t| probs[t as usize]).sum();
        Ok((self.no_speech.map_or(0.0, |t| probs[t as usize]), probs[self.english as usize], nordic))
    }
}

pub struct Transcriber {
    model: Model,
    /// A second, multilingual model used only to pick the language.
    language_id: Option<Model>,
    transcribe: u32,
    no_timestamps: u32,
    /// `<|startofprev|> Freya,`: previous-text context that teaches the decoder the name's
    /// spelling, so Norwegian decoding does not turn it into a look-alike ("Frøya", "Freia").
    prompt: Vec<u32>,
    /// Added to the logits: `-inf` for suppressed tokens (timestamps, specials, config list).
    suppress: Tensor,
}

impl Transcriber {
    /// `language_id`: a stock multilingual Whisper (for example `openai/whisper-tiny`). Without
    /// it every language is decoded and the caller picks.
    pub fn load(files: &WhisperFiles, language_id: Option<&WhisperFiles>, device: Device) -> Result<Self> {
        let model = Model::load(files, &device)?;
        let language_id = language_id.map(|f| Model::load(f, &device)).transpose()?;
        let id = |t: &str| model.vocab.special(t).with_context(|| format!("token {t} missing"));
        let (transcribe, no_timestamps) = (id(w::TRANSCRIBE_TOKEN)?, id(w::NO_TIMESTAMPS_TOKEN)?);
        let config = &model.whisper.config;
        let mut mask = vec![0f32; config.vocab_size];
        for (i, m) in mask.iter_mut().enumerate() {
            if i as u32 > model.eot || config.suppress_tokens.contains(&(i as u32)) {
                *m = f32::NEG_INFINITY;
            }
        }
        let suppress = Tensor::new(mask.as_slice(), &device)?;
        let prompt = Vec::new();
        Ok(Self { model, language_id, transcribe, no_timestamps, prompt, suppress })
    }

    /// Sets the spelling prompt for the wake word.
    pub fn with_wake_prompt(mut self, kind: WakePrompt, name: &str) -> Self {
        let vocab = &self.model.vocab;
        self.prompt = match (kind.text(name), vocab.special("<|startofprev|>")) {
            (Some(text), Some(prev)) => std::iter::once(prev).chain(vocab.encode_greedy(&text)).collect(),
            _ => Vec::new(),
        };
        self
    }

    /// Adds names the listener should spell right (rooms, lights, artists) to the prompt, after the
    /// wake word. Whisper keeps at most 224 prompt tokens; names past `max_tokens` are left out,
    /// so put the most important first.
    pub fn with_vocabulary(mut self, words: &[String], max_tokens: usize) -> Self {
        let vocab = &self.model.vocab;
        let Some(prev) = vocab.special("<|startofprev|>") else {
            return self;
        };
        if self.prompt.is_empty() {
            self.prompt.push(prev);
        }
        let budget = max_tokens.min(223).saturating_sub(self.prompt.len());
        let mut added = vocab.encode_greedy(" Names:");
        let mut first = true;
        for word in words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
            let piece = vocab.encode_greedy(&format!("{} {word}", if first { "" } else { "," }));
            if added.len() + piece.len() + 1 > budget {
                break;
            }
            added.extend(piece);
            first = false;
        }
        if !first {
            added.extend(vocab.encode_greedy("."));
            self.prompt.extend(added);
        }
        self
    }

    /// Prompt length in tokens.
    pub fn prompt_tokens(&self) -> usize {
        self.prompt.len()
    }

    /// The spelling prompt as text, for logging.
    pub fn prompt_text(&self) -> String {
        self.model.vocab.decode(&self.prompt)
    }

    /// Only this language is used from now on.
    pub fn restrict(&mut self, lang: Lang) {
        self.model.langs.retain(|(l, _)| *l == lang);
        self.language_id = None;
    }

    /// Transcripts, most likely first. `pcm` is 24 kHz mono. Empty when Whisper judges it not to
    /// be speech. One transcript when the language is known (language-ID model or a single
    /// language); otherwise one per language, ordered by confidence.
    ///
    /// NB-Whisper's own language prediction leans Norwegian and then translates English speech,
    /// which is why it is not used.
    pub fn transcribe(&mut self, pcm: &[i16]) -> Result<Vec<Transcript>> {
        let mut samples = resample_24k_to_16k(pcm);
        // Whisper is trained on 30 s windows; shorter input is padded with silence.
        samples.resize(w::N_SAMPLES, 0.0);

        let mut langs = self.model.langs.clone();
        if let Some(lid) = &mut self.language_id {
            let features = lid.features(&samples)?;
            let (no_speech, english, nordic) = lid.first_step(&features)?;
            if no_speech > NO_SPEECH_THRESHOLD {
                return Ok(Vec::new());
            }
            // whisper-tiny is sure about English (0.9+ with almost nothing Nordic on clear speech)
            // but scatters Norwegian over Danish, Swedish, German, English and more, so only a
            // clear English win counts as English.
            let clear_english = english >= ENGLISH_MIN && english >= ENGLISH_OVER_NORDIC * nordic;
            let lang = if clear_english { Lang::English } else { Lang::Norwegian };
            debug!(no_speech, english, nordic, ?lang, "language id");
            langs.retain(|(l, _)| *l == lang);
        }

        let features = self.model.features(&samples)?;
        if self.language_id.is_none() {
            let (no_speech, _, _) = self.model.first_step(&features)?;
            if no_speech > NO_SPEECH_THRESHOLD {
                return Ok(Vec::new());
            }
        }
        let mut out = Vec::new();
        for (lang, lang_token) in langs {
            let (text, score) = self.decode(lang_token, &features)?;
            debug!(?lang, score, text, "whisper candidate");
            if !text.is_empty() {
                out.push(Transcript { lang, text, score });
            }
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(out)
    }

    /// Greedy decode with the given language token. Returns the text and the mean log
    /// probability of its tokens.
    fn decode(&mut self, lang_token: u32, features: &Tensor) -> Result<(String, f32)> {
        let mut tokens = self.prompt.clone();
        tokens.extend([self.model.sot, lang_token, self.transcribe, self.no_timestamps]);
        let prefix = tokens.len();
        let mut logprob = 0f32;
        for i in 0..MAX_TOKENS {
            let logits = (self.model.logits(&tokens, features, i == 0)? + &self.suppress)?;
            let logp = candle_nn::ops::log_softmax(&logits, D::Minus1)?;
            let next = logits.argmax(D::Minus1)?.to_scalar::<u32>()?;
            logprob += logp.i(next as usize)?.to_scalar::<f32>()?;
            if next == self.model.eot {
                break;
            }
            tokens.push(next);
        }
        let count = (tokens.len() - prefix + 1) as f32;
        Ok((self.model.vocab.decode(&tokens[prefix..]), logprob / count))
    }
}

/// GPT-2 style byte-level BPE vocabulary, decoding only.
struct Vocab {
    tokens: Vec<String>,
    ids: HashMap<String, u32>,
    specials: HashMap<String, u32>,
    byte_of: HashMap<char, u8>,
    first_special: u32,
}

impl Vocab {
    fn load(path: &Path) -> Result<Self> {
        #[derive(serde::Deserialize)]
        struct Added {
            id: u32,
            content: String,
        }
        #[derive(serde::Deserialize)]
        struct Model {
            vocab: HashMap<String, u32>,
        }
        #[derive(serde::Deserialize)]
        struct File {
            model: Model,
            added_tokens: Vec<Added>,
        }
        let file: File = serde_json::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        let size = file.model.vocab.values().chain(file.added_tokens.iter().map(|a| &a.id)).max().map_or(0, |m| m + 1);
        let mut tokens = vec![String::new(); size as usize];
        for (token, id) in file.model.vocab {
            tokens[id as usize] = token;
        }
        let first_special = file.added_tokens.iter().map(|a| a.id).min().unwrap_or(size);
        let specials = file.added_tokens.into_iter().map(|a| (a.content, a.id)).collect();
        let byte_of = byte_to_char().into_iter().enumerate().map(|(b, c)| (c, b as u8)).collect();
        if tokens.is_empty() {
            bail!("empty vocabulary");
        }
        let ids = tokens.iter().enumerate().map(|(i, t)| (t.clone(), i as u32)).collect();
        Ok(Self { tokens, ids, specials, byte_of, first_special })
    }

    fn special(&self, token: &str) -> Option<u32> {
        self.specials.get(token).copied()
    }

    /// A regular vocabulary entry, in its byte-level spelling (`Ġ` for a leading space).
    /// Longest-match tokenization. Not the canonical BPE split, but a valid spelling of `text`,
    /// which is all a prompt needs.
    fn encode_greedy(&self, text: &str) -> Vec<u32> {
        let table = byte_to_char();
        let chars: Vec<char> = text.bytes().map(|b| table[b as usize]).collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let found = (i + 1..=chars.len().min(i + 24))
                .rev()
                .find_map(|j| self.ids.get(&chars[i..j].iter().collect::<String>()).map(|&id| (j, id)));
            match found {
                Some((j, id)) => {
                    out.push(id);
                    i = j;
                }
                None => i += 1,
            }
        }
        out
    }

    fn decode(&self, ids: &[u32]) -> String {
        let bytes: Vec<u8> = ids
            .iter()
            .filter(|&&id| id < self.first_special)
            .filter_map(|&id| self.tokens.get(id as usize))
            .flat_map(|t| t.chars())
            .filter_map(|c| self.byte_of.get(&c).copied())
            .collect();
        String::from_utf8_lossy(&bytes).trim().to_owned()
    }
}

/// GPT-2's reversible byte -> printable character table.
fn byte_to_char() -> [char; 256] {
    let printable = |b: u32| (0x21..=0x7e).contains(&b) || (0xa1..=0xac).contains(&b) || (0xae..=0xff).contains(&b);
    let mut table = ['\0'; 256];
    let mut extra = 0;
    for b in 0..256u32 {
        table[b as usize] = if printable(b) {
            char::from_u32(b).unwrap_or('\0')
        } else {
            extra += 1;
            char::from_u32(255 + extra).unwrap_or('\0')
        };
    }
    table
}

/// librosa `filters.mel(sr=16000, n_fft=400, n_mels, norm="slaney")`, row-major.
fn mel_filters(n_mels: usize) -> Vec<f32> {
    let bins = w::N_FFT / 2 + 1;
    let sr = w::SAMPLE_RATE as f64;
    let hz_to_mel = |hz: f64| {
        if hz < 1000.0 {
            hz * 3.0 / 200.0
        } else {
            15.0 + (hz / 1000.0).ln() / (6.4f64.ln() / 27.0)
        }
    };
    let mel_to_hz = |m: f64| {
        if m < 15.0 {
            m * 200.0 / 3.0
        } else {
            1000.0 * ((m - 15.0) * 6.4f64.ln() / 27.0).exp()
        }
    };
    let top = hz_to_mel(sr / 2.0);
    let points: Vec<f64> = (0..n_mels + 2).map(|i| mel_to_hz(top * i as f64 / (n_mels + 1) as f64)).collect();
    let mut out = vec![0f32; n_mels * bins];
    for m in 0..n_mels {
        let (lo, mid, hi) = (points[m], points[m + 1], points[m + 2]);
        let norm = 2.0 / (hi - lo);
        for k in 0..bins {
            let f = k as f64 * sr / w::N_FFT as f64;
            let weight = ((f - lo) / (mid - lo)).min((hi - f) / (hi - mid)).max(0.0);
            out[m * bins + k] = (weight * norm) as f32;
        }
    }
    out
}

/// 24 kHz -> 16 kHz (up 2, low-pass at 8 kHz, down 3), output scaled to [-1, 1].
pub fn resample_24k_to_16k(pcm: &[i16]) -> Vec<f32> {
    const TAPS: usize = 96;
    // Cutoff 7.2 kHz at the 48 kHz intermediate rate, Hann window, gain 2 for the zero stuffing.
    let h: Vec<f64> = (0..TAPS)
        .map(|i| {
            let x = i as f64 - (TAPS - 1) as f64 / 2.0;
            let fc = 7200.0 / 48000.0;
            let sinc =
                if x == 0.0 { 2.0 * fc } else { (std::f64::consts::TAU * fc * x).sin() / (std::f64::consts::PI * x) };
            let hann = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (TAPS - 1) as f64).cos();
            2.0 * sinc * hann
        })
        .collect();
    let out_len = pcm.len() * 2 / 3;
    (0..out_len)
        .map(|n| {
            // Upsampled index 3n + delay; only even upsampled indices carry input samples.
            let center = 3 * n + TAPS / 2;
            let mut acc = 0.0;
            for (k, tap) in h.iter().enumerate() {
                let Some(up) = center.checked_sub(k) else {
                    break;
                };
                if up % 2 == 0 {
                    if let Some(&s) = pcm.get(up / 2) {
                        acc += tap * f64::from(s);
                    }
                }
            }
            (acc / 32768.0) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_table_is_a_bijection() {
        let table = byte_to_char();
        let unique: std::collections::HashSet<char> = table.iter().copied().collect();
        assert_eq!(unique.len(), 256);
        assert_eq!(table[b'A' as usize], 'A');
        assert_eq!(table[b' ' as usize], '\u{120}');
    }

    #[test]
    fn resampler_keeps_a_1khz_tone() {
        let pcm: Vec<i16> = (0..24_000)
            .map(|i| ((i as f64 / 24_000.0 * 1000.0 * std::f64::consts::TAU).sin() * 10_000.0) as i16)
            .collect();
        let out = resample_24k_to_16k(&pcm);
        assert_eq!(out.len(), 16_000);
        let rms = (out[1000..15_000].iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / 14_000.0).sqrt();
        assert!((rms - 10_000.0 / 32768.0 / 2f64.sqrt()).abs() < 0.01, "rms {rms}");
        // Phase: 16 kHz sample n should match the 1 kHz sine at t = n / 16000.
        let expected = (1000.0 / 16_000.0 * 1000.0 * std::f64::consts::TAU).sin() * 10_000.0 / 32768.0;
        assert!((f64::from(out[1000]) - expected).abs() < 0.02, "{} vs {expected}", out[1000]);
    }
}
