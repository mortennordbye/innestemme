//! Speech output: Pocket TTS for English (see `pocket.rs`), macOS `say` for Norwegian until a
//! Norwegian model exists, and short chimes.

use anyhow::{bail, Context, Result};
use voice_proto::SAMPLE_RATE;

use crate::lang::Lang;

pub trait Tts: Send {
    /// Streams 24 kHz mono audio for `text` into `sink` as it is produced.
    fn speak(&mut self, text: &str, lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()>;

    /// Like `speak`, for text that changes from day to day without numbers (headlines): never
    /// kept by a speech cache.
    fn speak_live(&mut self, text: &str, lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        self.speak(text, lang, sink)
    }
}

/// One engine per language.
pub struct ByLanguage {
    pub english: Box<dyn Tts>,
    pub norwegian: Box<dyn Tts>,
}

impl Tts for ByLanguage {
    fn speak(&mut self, text: &str, lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        match lang {
            Lang::English => self.english.speak(text, lang, sink),
            Lang::Norwegian => self.norwegian.speak(text, lang, sink),
        }
    }

    fn speak_live(&mut self, text: &str, lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        match lang {
            Lang::English => self.english.speak_live(text, lang, sink),
            Lang::Norwegian => self.norwegian.speak_live(text, lang, sink),
        }
    }
}

/// macOS `say`, rendered to a temporary wav file. Not streaming.
pub struct Say {
    /// The system default voice when `None`.
    pub english: Option<String>,
    pub norwegian: String,
}

impl Tts for Say {
    fn speak(&mut self, text: &str, lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        let path = std::env::temp_dir().join(format!("voice-say-{}.wav", std::process::id()));
        let mut cmd = std::process::Command::new("say");
        cmd.arg("-o").arg(&path).arg("--file-format=WAVE").arg(format!("--data-format=LEI16@{SAMPLE_RATE}"));
        let voice = match lang {
            Lang::English => self.english.as_ref(),
            Lang::Norwegian => Some(&self.norwegian),
        };
        if let Some(voice) = voice {
            cmd.arg("-v").arg(voice);
        }
        let status = cmd.arg("--").arg(text).status().context("running `say` (macOS only)")?;
        if !status.success() {
            bail!("`say` exited with {status}");
        }
        let mut reader = hound::WavReader::open(&path).with_context(|| format!("reading {}", path.display()))?;
        let spec = reader.spec();
        if spec.sample_rate != SAMPLE_RATE || spec.channels != 1 || spec.bits_per_sample != 16 {
            bail!("`say` wrote {spec:?}, expected 24 kHz mono 16-bit");
        }
        let pcm = reader.samples::<i16>().collect::<Result<Vec<_>, _>>()?;
        let _ = std::fs::remove_file(&path);
        sink(&pcm);
        Ok(())
    }
}

/// Three quick rising tones: a session started and the microphone is live.
pub fn ready_chime() -> Vec<i16> {
    tones(&[(660.0, 0.07), (880.0, 0.07), (1100.0, 0.1)])
}

/// Two rising tones: the assistant heard its name and is listening.
pub fn wake_chime() -> Vec<i16> {
    tones(&[(880.0, 0.09), (1320.0, 0.12)])
}

/// One falling tone: nothing was asked after the wake word.
pub fn give_up_chime() -> Vec<i16> {
    tones(&[(660.0, 0.08), (440.0, 0.14)])
}

/// A timer ran out: three bright double beeps, about two seconds.
pub fn alarm_chime() -> Vec<i16> {
    let beeps = [(1319.0, 0.12), (0.0, 0.06), (1319.0, 0.12), (0.0, 0.4)];
    tones(&beeps.repeat(3))
}

/// A reminder is about to be spoken.
pub fn reminder_chime() -> Vec<i16> {
    tones(&[(784.0, 0.1), (1047.0, 0.1), (1319.0, 0.16), (0.0, 0.2)])
}

fn tones(parts: &[(f32, f32)]) -> Vec<i16> {
    let rate = SAMPLE_RATE as f32;
    let mut out = Vec::new();
    for &(hz, seconds) in parts {
        let n = (seconds * rate) as usize;
        let fade = (0.01 * rate) as usize;
        out.extend((0..n).map(|i| {
            let envelope = (i.min(n - 1 - i) as f32 / fade as f32).min(1.0);
            ((i as f32 / rate * hz * std::f32::consts::TAU).sin() * 7000.0 * envelope) as i16
        }));
    }
    out
}
