//! Voice assistant pieces behind the engine's `assistant` processor: streaming speech-to-text, the
//! wake word and turn logic, intent parsing, tools and speech output.

pub mod catalogue;
pub mod dialog;
pub mod geo;
pub mod ha;
pub mod intent;
pub mod jokes;
pub mod kokoro;
pub mod lang;
pub mod llm;
pub mod music;
pub mod names;
pub mod persona;
pub mod pocket;
pub mod power;
pub mod resample;
pub mod shopping;
pub mod speech_cache;
pub mod spm;
pub mod stt;
pub mod timer;
pub mod transit;
pub mod tts;
pub mod vad;
pub mod weather;
pub mod whisper;
mod whisper_decoder;
pub mod wyoming;
