//! Speech rendered once and played back after: fixed sentences ("Very good, sir.", the lead-ins,
//! jokes, "The kitchen lights are off.") cost nothing after the first time, so on a slow CPU only
//! the sentence with live data is synthesized. Sentences with digits are live data and are not
//! stored, except a few frequent shapes with small numbers ("Volume 60 percent.", "Timer set for 25
//! minutes."), which come up again and again. English only; Norwegian goes straight through.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use crate::lang::Lang;
use crate::tts::Tts;

/// Sentences with numbers that are kept anyway, digits as `#`, and the largest number each may
/// hold: the answers with live data that come up most, so they need not be synthesized each time.
/// The limits keep what is stored bounded.
const NUMBER_SHAPES: &[(&str, u32)] = &[
    ("volume # percent", 100),
    ("timer set for #", 100),
    ("i'll remind you in #", 100),
    ("the # minute timer is", 100),
    ("the # minutes timer is", 100),
    ("the # second timer is", 100),
    ("the # hour timer is", 100),
    // Room temperatures and the weather: "It's 21 degrees in the bedroom.", "It's 4 degrees in Oslo."
    ("it's # degrees in ", 100),
    ("it's # degree in ", 100),
    ("the high today is #", 100),
    ("# to # degrees", 100),
    // The time: "It's 16:50."
    ("it's #:#", 59),
    // Departures: the line ("The next bus is the 31 to Snarøya, from Wessels plass."), then when.
    ("is the # to ", 999),
    ("departure is ", 999),
    ("it leaves in #", 59),
    ("it leaves at #:#", 59),
    ("the one after leaves in #", 59),
    ("the one after leaves at #:#", 59),
    // Electricity prices: "Right now, power costs 1.44 kroner per kilowatt hour."
    ("power costs #", 100),
    ("it costs #", 100),
    (" is at #:#.", 59),
];

pub struct CachedTts {
    inner: Box<dyn Tts>,
    /// One directory per voice: the audio is that voice's.
    dir: Option<PathBuf>,
    memory: HashMap<String, Arc<[i16]>>,
}

impl CachedTts {
    /// `dir` keeps the audio across restarts; without it, the cache lives as long as the engine.
    pub fn new(inner: Box<dyn Tts>, dir: Option<PathBuf>) -> Self {
        if let Some(dir) = &dir {
            if let Err(error) = std::fs::create_dir_all(dir) {
                warn!(%error, dir = %dir.display(), "speech cache directory not usable; keeping speech in memory only");
            }
        }
        Self { inner, dir, memory: HashMap::new() }
    }

    /// Whether a sentence is worth keeping: fixed text, or one of the [`NUMBER_SHAPES`] within its
    /// limit, so what is kept stays bounded.
    pub fn cacheable(text: &str) -> bool {
        let numbers: Vec<u32> = text
            .split(|c: char| !c.is_ascii_digit())
            .filter(|n| !n.is_empty())
            .map(|n| n.parse().unwrap_or(u32::MAX))
            .collect();
        if numbers.is_empty() {
            return true;
        }
        let mut shape = String::with_capacity(text.len());
        for c in text.to_lowercase().chars() {
            match (c.is_ascii_digit(), shape.ends_with('#')) {
                (true, true) => {}
                (true, false) => shape.push('#'),
                (false, _) => shape.push(c),
            }
        }
        // "It's minus 4 degrees in Oslo." has the shape of "It's 4 degrees in Oslo."
        let shape = shape.replace("minus #", "#");
        let largest = numbers.into_iter().max().unwrap_or(0);
        NUMBER_SHAPES.iter().any(|(s, max)| largest <= *max && shape.contains(s))
    }

    fn path(&self, text: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join(format!("{:016x}.pcm", fnv1a(text))))
    }

    fn load(&mut self, text: &str) -> Option<Arc<[i16]>> {
        if let Some(pcm) = self.memory.get(text) {
            return Some(pcm.clone());
        }
        let bytes = std::fs::read(self.path(text)?).ok()?;
        let (samples, _) = bytes.as_chunks::<2>();
        let pcm: Arc<[i16]> = samples.iter().map(|b| i16::from_le_bytes(*b)).collect();
        self.memory.insert(text.to_owned(), pcm.clone());
        Some(pcm)
    }

    fn store(&mut self, text: &str, pcm: Vec<i16>) {
        if let Some(path) = self.path(text) {
            let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
            // Written aside and renamed, so a crash never leaves half a sentence to play back.
            let partial = path.with_extension("part");
            if let Err(error) = std::fs::write(&partial, bytes).and_then(|()| std::fs::rename(&partial, &path)) {
                warn!(%error, "could not store speech");
            }
        }
        self.memory.insert(text.to_owned(), pcm.into());
    }
}

impl CachedTts {
    /// `ahead`: rendered while idle, so a sentence not kept yet is expected, not a miss.
    fn cached(&mut self, text: &str, lang: Lang, ahead: bool, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        let text = text.trim();
        if lang != Lang::English || !Self::cacheable(text) {
            return self.inner.speak(text, lang, sink);
        }
        if let Some(pcm) = self.load(text) {
            sink(&pcm);
            return Ok(());
        }
        if !ahead {
            // Each of these was heard only after it was synthesized: a sentence to render ahead.
            info!(text, "speech not rendered ahead");
        }
        let mut pcm = Vec::new();
        self.inner.speak(text, lang, &mut |chunk| {
            pcm.extend_from_slice(chunk);
            sink(chunk);
        })?;
        if !pcm.is_empty() {
            self.store(text, pcm);
        }
        Ok(())
    }
}

impl Tts for CachedTts {
    fn speak(&mut self, text: &str, lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        self.cached(text, lang, false, sink)
    }

    fn speak_live(&mut self, text: &str, lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        self.inner.speak(text, lang, sink)
    }

    fn render(&mut self, text: &str, lang: Lang) -> Result<()> {
        self.cached(text, lang, true, &mut |_| {})
    }
}

/// A stable hash for file names: the standard library's may change between Rust versions.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_frequent_number_sentences_only() {
        assert!(CachedTts::cacheable("Very good, sir."));
        assert!(CachedTts::cacheable("Volume 60 percent."));
        assert!(CachedTts::cacheable("Timer set for 1 hour and 30 minutes, sir."));
        assert!(CachedTts::cacheable("The 5 minute timer is cancelled."));
        assert!(CachedTts::cacheable("It's 21 degrees in the bedroom."));
        assert!(CachedTts::cacheable("It's 06:38."));
        assert!(CachedTts::cacheable("It's 16:50, sir."));
        assert!(!CachedTts::cacheable("37 seconds left on the 1 minute timer."));
        assert!(!CachedTts::cacheable("Timer set for 120 minutes."));
        assert!(!CachedTts::cacheable("It's 5 degrees and partly cloudy in Oslo, with a high of 13."));
        assert!(CachedTts::cacheable("It's minus 4 degrees in Oslo."));
        assert!(CachedTts::cacheable("Minus 3 to 12 degrees."));
        assert!(CachedTts::cacheable("The next bus is the 390 to Snarøya, from Wessels plass."));
        assert!(CachedTts::cacheable("It leaves in 4 minutes."));
        assert!(CachedTts::cacheable("Right now, power costs 1.44 kroner per kilowatt hour."));
        assert!(CachedTts::cacheable("Cheapest tonight is at 03:15."));
        assert!(!CachedTts::cacheable("It leaves in 75 minutes."));
        assert!(!CachedTts::cacheable("Today it ranges from 36 øre to 2.10 kroner."));
    }

    /// Counts how often it is asked to speak; says one sample per character.
    struct Counting(Arc<std::sync::atomic::AtomicUsize>);

    impl Tts for Counting {
        fn speak(&mut self, text: &str, _lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            sink(&vec![7; text.len()]);
            Ok(())
        }
    }

    fn heard(tts: &mut CachedTts, text: &str) -> usize {
        let mut n = 0;
        tts.speak(text, Lang::English, &mut |pcm| n += pcm.len()).unwrap();
        n
    }

    #[test]
    fn fixed_sentences_are_rendered_once_and_kept_on_disk() {
        let dir = std::env::temp_dir().join(format!("speech-cache-test-{}", std::process::id()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tts = CachedTts::new(Box::new(Counting(calls.clone())), Some(dir.clone()));
        assert_eq!(heard(&mut tts, "Very good, sir."), 15);
        assert_eq!(heard(&mut tts, "Very good, sir."), 15);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        // Live data is spoken every time.
        heard(&mut tts, "It's 4 degrees.");
        heard(&mut tts, "It's 4 degrees.");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
        // Frequent shapes with small numbers are kept.
        heard(&mut tts, "Volume 60 percent, sir.");
        heard(&mut tts, "Volume 60 percent, sir.");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 4);
        // A restart finds the fixed sentence on disk.
        let mut again = CachedTts::new(Box::new(Counting(calls.clone())), Some(dir.clone()));
        assert_eq!(heard(&mut again, "Very good, sir."), 15);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 4);
        let _ = std::fs::remove_dir_all(dir);
    }
}
