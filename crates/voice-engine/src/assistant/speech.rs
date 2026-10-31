//! Speaking answers: synthesis a sentence at a time, the follow-up window, announcements.

use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use voice_assistant::house;
use voice_assistant::lang::Lang;
use voice_proto::SAMPLE_RATE;

use crate::activity;

use super::follow_up::ECHO_WINDOW;
use super::{AssistantEvent, Worker, FOLLOW_UP};

/// From queuing an answer to the speaker having played it: its length and this for the player to
/// start. Music started or stopped sooner cuts the announcement off.
const ANSWER_START: Duration = Duration::from_millis(1500);

/// Where a sentence's speech starts (about -38 dBFS, above Kokoro's noise floor), and how much before
/// that is kept (10 ms).
const SPEECH_ONSET: u16 = 400;

const ONSET_LEAD: usize = SAMPLE_RATE as usize / 100;

/// What an answer has put out so far.
#[derive(Default)]
pub(super) struct Spoken {
    samples: usize,
    first: Option<Instant>,
}

pub(super) fn push(reply: &mut rtrb::Producer<i16>, pcm: &[i16]) {
    let n = pcm.len().min(reply.slots());
    if let Ok(chunk) = reply.write_chunk_uninit(n) {
        chunk.fill_from_iter(pcm[..n].iter().copied());
    }
}

impl Worker {
    /// Speaks an answer and opens the follow-up window.
    pub(super) fn say(&mut self, answer: String, lang: Lang, start: Instant) {
        self.interrupted.store(false, Relaxed);
        let answer = match &mut self.persona {
            Some(persona) if lang == Lang::English => persona.style(&answer, jiff::Zoned::now().hour() as u8),
            _ => answer,
        };
        self.emit(AssistantEvent::Answer { text: answer.clone() });
        let mut spoken = Spoken::default();
        self.speak(&answer, lang, &mut spoken);
        self.finish(spoken, start);
    }

    /// Audio goes to the speaker as it is synthesized, a sentence at a time.
    pub(super) fn speak(&mut self, text: &str, lang: Lang, spoken: &mut Spoken) {
        self.said.retain(|(at, _)| at.elapsed() < ECHO_WINDOW);
        self.said.push_back((Instant::now(), text.to_owned()));
        let (reply, events, tts, live, interrupted) =
            (&mut self.reply, &self.events, &mut self.tts, &self.live, &self.interrupted);
        for sentence in text.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty()) {
            if interrupted.load(Relaxed) {
                info!("answer interrupted");
                break;
            }
            let dark = house::threat_sentence(sentence);
            // The dark voice trims its own onset and keeps the pause it opens with.
            let mut started = dark;
            let mut sink = |pcm: &[i16]| {
                // The rest of the sentence being synthesized when the answer was interrupted.
                if interrupted.load(Relaxed) {
                    return;
                }
                // Kokoro opens each sentence with 50-90 ms of silence, heard as a slower answer.
                let pcm = match started {
                    true => pcm,
                    false => match pcm.iter().position(|s| s.unsigned_abs() > SPEECH_ONSET) {
                        Some(onset) => &pcm[onset.saturating_sub(ONSET_LEAD)..],
                        None => return,
                    },
                };
                started = true;
                spoken.first.get_or_insert_with(Instant::now);
                spoken.samples += pcm.len();
                push(reply, pcm);
                let _ = events.send(AssistantEvent::Speech(pcm.into()));
            };
            let result = if dark {
                // Whole, so the effect can stretch it: from the cache, it takes no time.
                let mut pcm = Vec::new();
                let result = tts.speak(sentence, lang, &mut |p| pcm.extend_from_slice(p));
                let start = pcm.iter().position(|s| s.unsigned_abs() > SPEECH_ONSET).unwrap_or(pcm.len());
                sink(&voice_assistant::tts::darken(&pcm[start.saturating_sub(ONSET_LEAD)..]));
                result
            } else if live.contains(sentence) {
                tts.speak_live(sentence, lang, &mut sink)
            } else {
                tts.speak(sentence, lang, &mut sink)
            };
            if let Err(error) = result {
                warn!(%error, "speech synthesis failed");
            }
        }
    }

    pub(super) fn finish(&mut self, spoken: Spoken, start: Instant) {
        self.woken_at = None;
        self.live.clear();
        let seconds = spoken.samples as f32 / SAMPLE_RATE as f32;
        // `after_speech`: from the end of the speaker's words to the first audio of the answer.
        let after_speech = self.speech_ended.take().zip(spoken.first).map(|(ended, first)| first - ended);
        let first_audio_after = spoken.first.map(|first| first - start);
        info!(?first_audio_after, ?after_speech, done_after = ?start.elapsed(), seconds, "spoken");
        if let Some(turn) = self.turn.take() {
            self.config
                .activity
                .record(activity::Turn { after_ms: after_speech.map(|d| d.as_millis() as u64), ..turn });
        }
        self.emit(AssistantEvent::Spoken);
        // After the answer is queued: the speaker's announcement ducks the music under it.
        self.unhush();
        // The reply is queued faster than it plays; the window opens when it has been heard.
        self.follow_up_until = Some(Instant::now() + Duration::from_secs_f32(seconds) + FOLLOW_UP);
        if self.after_answer.is_some() {
            // The lockdown's music sets the scene under its threat; other music waits for the answer.
            let wait = if self.music_under_answer { Duration::ZERO } else { Duration::from_secs_f32(seconds) };
            self.after_answer_at = Some(Instant::now() + wait + ANSWER_START);
        }
        self.busy.store(false, Relaxed);
    }

    /// Speaks outside a conversation, after a chime. The audio also goes to satellites as an
    /// announcement.
    pub(super) fn announce(&mut self, text: &str, chime: Vec<i16>, lang: Lang, timer_ring: bool) {
        // Deaf while it plays, like an answer.
        self.busy.store(true, Relaxed);
        let mut audio = chime;
        if let Err(error) = self.tts.speak(text, lang, &mut |pcm| audio.extend_from_slice(pcm)) {
            warn!(%error, "speech synthesis failed");
        }
        push(&mut self.reply, &audio);
        self.busy.store(false, Relaxed);
        self.emit(AssistantEvent::Announcement { text: text.to_owned(), audio: audio.into(), timer_ring });
    }

    pub(super) fn play(&mut self, pcm: &[i16]) {
        push(&mut self.reply, pcm);
    }
}
