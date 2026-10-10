//! Speech rendered once and played back after: fixed sentences ("Very good, sir.", the lead-ins,
//! jokes, "The kitchen lights are off.") cost nothing after the first time, so on a slow CPU only
//! the sentence with live data is synthesized. Sentences with digits are live data and are never
//! stored. English only; Norwegian goes straight through.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::warn;

use crate::lang::Lang;
use crate::tts::Tts;

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

    /// Whether a sentence is fixed text worth keeping: numbers are live data.
    pub fn cacheable(text: &str) -> bool {
        !text.chars().any(|c| c.is_ascii_digit())
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

impl Tts for CachedTts {
    fn speak(&mut self, text: &str, lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        let text = text.trim();
        if lang != Lang::English || !Self::cacheable(text) {
            return self.inner.speak(text, lang, sink);
        }
        if let Some(pcm) = self.load(text) {
            sink(&pcm);
            return Ok(());
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

/// A stable hash for file names: the standard library's may change between Rust versions.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // A restart finds the fixed sentence on disk.
        let mut again = CachedTts::new(Box::new(Counting(calls.clone())), Some(dir.clone()));
        assert_eq!(heard(&mut again, "Very good, sir."), 15);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
        let _ = std::fs::remove_dir_all(dir);
    }
}
