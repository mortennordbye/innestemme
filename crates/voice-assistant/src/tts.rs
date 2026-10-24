//! Speech output: the engines behind one trait (Kokoro in `kokoro.rs`, Piper in `wyoming.rs`,
//! macOS `say` here), one per language, and short chimes.

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

    /// Synthesizes `text` for later, unheard: a speech cache keeps it.
    fn render(&mut self, text: &str, lang: Lang) -> Result<()> {
        self.speak(text, lang, &mut |_| {})
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

    fn render(&mut self, text: &str, lang: Lang) -> Result<()> {
        match lang {
            Lang::English => self.english.render(text, lang),
            Lang::Norwegian => self.norwegian.render(text, lang),
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

/// The dark voice of the lockdown's threats: slowed and lowered by a fifth of the speed (about
/// four semitones), muffled, with a dry echo of a large room, after a dramatic pause.
pub fn darken(pcm: &[i16]) -> Vec<i16> {
    const SPEED: f32 = 0.8;
    let rate = SAMPLE_RATE as f32;
    let len = (pcm.len() as f32 / SPEED) as usize;
    // Read slower than written: lower and slower together, which is what makes it sound menacing.
    let slowed = (0..len).map(|i| {
        let at = i as f32 * SPEED;
        let (j, frac) = (at as usize, at.fract());
        let a = pcm.get(j).copied().unwrap_or(0) as f32;
        let b = pcm.get(j + 1).copied().unwrap_or(0) as f32;
        a + (b - a) * frac
    });
    // A one-pole low-pass at about 3 kHz takes the brightness off.
    let k = 1.0 - (-std::f32::consts::TAU * 3000.0 / rate).exp();
    let mut low = 0.0;
    let mut dry: Vec<f32> = slowed
        .map(|x| {
            low += k * (x - low);
            low
        })
        .collect();
    // The echo needs room to ring out.
    dry.resize(dry.len() + (0.6 * rate) as usize, 0.0);
    let mut wet = dry.clone();
    for (delay, gain) in [(0.061, 0.32), (0.089, 0.24), (0.137, 0.18), (0.211, 0.12)] {
        let d = (delay * rate) as usize;
        for i in d..wet.len() {
            wet[i] += wet[i - d] * gain;
        }
    }
    let mixed: Vec<f32> = dry.iter().zip(&wet).map(|(d, w)| d * 0.75 + w * 0.4).collect();
    let peak = mixed.iter().fold(1.0f32, |m, x| m.max(x.abs()));
    let target = pcm.iter().fold(1.0f32, |m, &x| m.max((x as f32).abs()));
    let gain = target / peak;
    let pause = vec![0i16; (0.7 * rate) as usize];
    pause.into_iter().chain(mixed.into_iter().map(|x| (x * gain) as i16)).collect()
}

/// Three quick rising tones: a session started and the microphone is live.
pub fn ready_chime() -> Vec<i16> {
    tones(&[(660.0, 0.07), (880.0, 0.07), (1100.0, 0.1)])
}

/// Two rising tones: the assistant heard its name and is listening.
pub fn wake_chime() -> Vec<i16> {
    tones(&[(880.0, 0.09), (1320.0, 0.12)])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn darkened_speech_is_longer_and_no_louder() {
        let speech: Vec<i16> = (0..SAMPLE_RATE as usize).map(|i| ((i as f32 * 0.05).sin() * 20000.0) as i16).collect();
        let dark = darken(&speech);
        assert!(dark.len() > speech.len() * 5 / 4);
        let peak = |pcm: &[i16]| pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(peak(&dark) <= peak(&speech));
        assert!(peak(&dark) > peak(&speech) / 2);
        assert!(dark[..SAMPLE_RATE as usize / 2].iter().all(|&s| s == 0), "a pause first");
    }
}
