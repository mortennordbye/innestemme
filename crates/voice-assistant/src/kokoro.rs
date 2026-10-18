//! Kokoro speech, English only, at 24 kHz (the engine's own rate). Two ways: Kokoro-FastAPI's
//! OpenAI-style endpoint (`/v1/audio/speech`), raw PCM streamed as it arrives; or the model in the
//! engine through sherpa-onnx ([`KokoroOnnx`]), with no server and about a quarter faster on the same
//! CPUs (docs/benchmarks.md).

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::json;
use sherpa_onnx::{GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig, OfflineTtsModelConfig};

use crate::lang::Lang;
use crate::tts::Tts;

/// A long answer on a slow CPU still finishes well inside this.
const TIMEOUT: Duration = Duration::from_secs(30);
/// 100 ms of audio per read.
const CHUNK_BYTES: usize = 4800;

pub struct KokoroTts {
    url: String,
    voice: String,
    agent: ureq::Agent,
}

impl KokoroTts {
    /// `url` is the server's base, e.g. http://127.0.0.1:8880.
    pub fn new(url: &str, voice: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_owned(),
            voice: voice.to_owned(),
            agent: ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into(),
        }
    }

    /// The voices the server offers, for a start-up check.
    pub fn voices(&self) -> Result<Vec<String>> {
        let url = format!("{}/v1/audio/voices", self.url);
        let reply: serde_json::Value =
            self.agent.get(&url).call().with_context(|| format!("asking Kokoro at {url}"))?.body_mut().read_json()?;
        Ok(reply["voices"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect())
    }

    pub fn voice(&self) -> &str {
        &self.voice
    }
}

impl Tts for KokoroTts {
    fn speak(&mut self, text: &str, _lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        let body = json!({
            "model": "kokoro",
            "input": text,
            "voice": self.voice,
            "response_format": "pcm",
            "stream": true,
        });
        let response =
            self.agent.post(format!("{}/v1/audio/speech", self.url)).send_json(body).context("Kokoro speech")?;
        let mut reader = response.into_body().into_reader();
        let (mut buf, mut held) = (vec![0u8; CHUNK_BYTES + 1], 0usize);
        let mut any = false;
        loop {
            let n = reader.read(&mut buf[held..]).context("reading Kokoro audio")?;
            if n == 0 {
                break;
            }
            let have = held + n;
            // A read can end in the middle of a sample; keep that byte for the next one.
            let whole = have & !1;
            let (samples, _) = buf[..whole].as_chunks::<2>();
            let pcm: Vec<i16> = samples.iter().map(|b| i16::from_le_bytes(*b)).collect();
            if !pcm.is_empty() {
                any = true;
                sink(&pcm);
            }
            held = have - whole;
            if held == 1 {
                buf[0] = buf[whole];
            }
        }
        if !any {
            bail!("Kokoro sent no audio");
        }
        Ok(())
    }
}

/// sherpa-onnx's export of Kokoro v1.0, the model Kokoro-FastAPI runs.
pub const ONNX_REPO: &str = "csukuangfj/kokoro-multi-lang-v1_0";

/// The English voices of Kokoro v1.0, in the order of its voice table (`speaker2id` in the model's
/// metadata).
const VOICES: &[&str] = &[
    "af_alloy",
    "af_aoede",
    "af_bella",
    "af_heart",
    "af_jessica",
    "af_kore",
    "af_nicole",
    "af_nova",
    "af_river",
    "af_sarah",
    "af_sky",
    "am_adam",
    "am_echo",
    "am_eric",
    "am_fenrir",
    "am_liam",
    "am_michael",
    "am_onyx",
    "am_puck",
    "am_santa",
    "bf_alice",
    "bf_emma",
    "bf_isabella",
    "bf_lily",
    "bm_daniel",
    "bm_fable",
    "bm_george",
    "bm_lewis",
];

/// sherpa-onnx's export of Kokoro v1.0, downloaded into the Hugging Face cache: the f32 model (the
/// int8 one is about three times slower on a CPU), the voices and English pronunciation.
pub struct KokoroFiles {
    dir: PathBuf,
}

impl KokoroFiles {
    pub fn fetch(repo: &str) -> Result<Self> {
        let api = hf_hub::api::sync::ApiBuilder::from_env().build()?;
        let repo_api = api.model(repo.to_owned());
        let info = repo_api.info().with_context(|| format!("listing {repo}"))?;
        // espeak-ng's phoneme tables and English rules; not its other languages, nor its voice
        // variants, some of whose names ("!v/Mr serious") hf-hub cannot fetch.
        let wanted = |f: &str| {
            let espeak = f.strip_prefix("espeak-ng-data/").unwrap_or_default();
            ["model.onnx", "voices.bin", "tokens.txt", "lexicon-us-en.txt", "lexicon-gb-en.txt"].contains(&f)
                || ["phondata", "phondata-manifest", "phonindex", "phontab", "intonations", "en_dict"].contains(&espeak)
                || espeak.starts_with("lang/")
        };
        let mut model = None;
        for file in info.siblings.iter().map(|s| s.rfilename.as_str()).filter(|f| wanted(f)) {
            let path = repo_api.get(file).with_context(|| format!("fetching {repo}/{file}"))?;
            if file == "model.onnx" {
                model = Some(path);
            }
        }
        let model = model.with_context(|| format!("{repo} has no model.onnx"))?;
        Ok(Self { dir: model.parent().context("model.onnx without a folder")?.to_owned() })
    }

    pub fn model(&self) -> PathBuf {
        self.dir.join("model.onnx")
    }
}

pub struct KokoroOnnx {
    tts: OfflineTts,
    generation: GenerationConfig,
}

impl KokoroOnnx {
    /// `voice` is one of Kokoro's English voices, e.g. `am_onyx`; British ones (`b...`) get British
    /// pronunciation.
    pub fn load(files: &KokoroFiles, voice: &str, threads: usize) -> Result<Self> {
        let sid = VOICES
            .iter()
            .position(|v| *v == voice)
            .with_context(|| format!("Kokoro has no English voice {voice}; one of {}", VOICES.join(", ")))?;
        let british = voice.starts_with('b');
        let file = |name: &str| Some(files.dir.join(name).to_string_lossy().into_owned());
        let config = OfflineTtsConfig {
            model: OfflineTtsModelConfig {
                kokoro: OfflineTtsKokoroModelConfig {
                    model: file("model.onnx"),
                    voices: file("voices.bin"),
                    tokens: file("tokens.txt"),
                    data_dir: file("espeak-ng-data"),
                    lexicon: file(if british { "lexicon-gb-en.txt" } else { "lexicon-us-en.txt" }),
                    lang: Some(if british { "en-gb" } else { "en-us" }.into()),
                    ..Default::default()
                },
                num_threads: threads.max(1) as i32,
                ..Default::default()
            },
            max_num_sentences: 1,
            ..Default::default()
        };
        let tts = OfflineTts::create(&config).context("loading Kokoro")?;
        anyhow::ensure!(tts.sample_rate() == 24_000, "Kokoro speaks at {} Hz, not 24 kHz", tts.sample_rate());
        let generation = GenerationConfig { sid: sid as i32, ..Default::default() };
        // The first call sets up buffers; it is not what a sentence costs.
        tts.generate_with_config::<fn(&[f32], f32) -> bool>("Hello.", &generation, None);
        Ok(Self { tts, generation })
    }
}

impl Tts for KokoroOnnx {
    fn speak(&mut self, text: &str, _lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        let audio = self
            .tts
            .generate_with_config::<fn(&[f32], f32) -> bool>(text, &self.generation, None)
            .context("Kokoro made no audio")?;
        let pcm: Vec<i16> = audio.samples().iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
        if pcm.is_empty() {
            bail!("Kokoro made no audio");
        }
        sink(&pcm);
        Ok(())
    }
}
