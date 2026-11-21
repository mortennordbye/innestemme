//! Setup that several examples share. Each example compiles this module on its own and uses only
//! part of it, so the rest is dead code there.
#![allow(dead_code)]

use std::sync::Arc;

use anyhow::Result;
use voice_assistant::parakeet::{Parakeet, ParakeetFiles, DEFAULT_REPO};
use voice_assistant::wakeword::{WakeDetector, WakeModels, CHUNK};
use voice_assistant::whisper::{default_repo, WhisperFiles};

/// The Whisper files WHISPER_SIZE names (default base).
pub fn whisper_from_env() -> Result<WhisperFiles> {
    let size = std::env::var("WHISPER_SIZE").unwrap_or_else(|_| "base".into());
    WhisperFiles::fetch(&default_repo(false, &size))
}

/// The Parakeet model PARAKEET names (default the engine's), on 4 threads.
pub fn parakeet_from_env() -> Result<Parakeet> {
    Parakeet::load(&ParakeetFiles::fetch(&std::env::var("PARAKEET").unwrap_or_else(|_| DEFAULT_REPO.into()))?, 4)
}

/// The highest wake word score in 16 kHz `pcm`, and the sample count at the end of the chunk that
/// scored it.
pub fn wake_peak(models: &Arc<WakeModels>, pcm: &[i16]) -> Result<(f32, usize)> {
    let mut detector = WakeDetector::new(models.clone());
    let (mut best, mut at, mut offset) = (0.0f32, 0usize, 0usize);
    for block in pcm.chunks(CHUNK) {
        offset += block.len();
        if let Some(score) = detector.push(block)? {
            if score > best {
                (best, at) = (score, offset);
            }
        }
    }
    Ok((best, at))
}
