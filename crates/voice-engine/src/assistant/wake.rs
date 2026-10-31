//! Wake words: whether the name was in them, and the reply when what follows is not understood.

use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use voice_assistant::lang::Lang;
use voice_assistant::voice_gender::Gender;
use voice_proto::SAMPLE_RATE;

use crate::activity::Outcome;
use crate::unhandled::Reason;

use super::follow_up::sounds_like_a_request;
use super::hush::Hushed;
use super::{AssistantEvent, Worker};

/// A short utterance that starts this soon after a device's wake word holds the end of the wake
/// word ("Oh", "What?"), not the request. A longer one is the name and the request in one breath.
const WAKE_TAIL: Duration = Duration::from_millis(500);
const WAKE_TAIL_LONGEST: Duration = Duration::from_millis(1600);

/// Unrecognised speech after a wake longer than this many words gets no "didn't catch that".
const LONGEST_REQUEST_WORDS: usize = 12;

/// What a wake word tells of the assistant's name being said.
pub(super) enum WakeName {
    /// The device's own wake word: no audio.
    Unknown,
    /// The engine's, with its audio (24 kHz) to check.
    Check(Vec<i16>),
    /// A near miss already checked: the name was in it.
    Heard,
}

impl Worker {
    /// A device woke on its wake word and the request was not understood: say so, once, rather
    /// than end the run in silence, which sounds as if the device never woke.
    /// Whether a wake word's audio holds the assistant's name.
    pub(super) fn name_in(&mut self, audio: &[i16]) -> Option<bool> {
        let transcriber = match &mut self.wake_listener {
            Some(listener) => listener,
            None => &mut self.transcriber,
        };
        let start = Instant::now();
        let heard = match transcriber.transcribe(audio) {
            Ok(candidates) => candidates.into_iter().next().map(|c| c.text).unwrap_or_default(),
            Err(error) => {
                warn!(%error, "could not transcribe the wake word");
                return None;
            }
        };
        let wake = &self.config.wake;
        let named = wake.in_wake_word(&heard);
        info!(heard, named, took = ?start.elapsed(), "wake word check");
        Some(named)
    }

    pub(super) fn not_understood(&mut self, text: &str, audio: &[i16], ended: Instant, lang: Lang, start: Instant) {
        let Some(woken) = self.woken_at else {
            if self.follow_up_until.is_some_and(|until| Instant::now() < until) {
                self.unhandled(Reason::FollowUpIgnored, text, None);
            }
            return;
        };
        let length = Duration::from_secs_f32(audio.len() as f32 / SAMPLE_RATE as f32);
        let began = ended.checked_sub(length);
        if length < WAKE_TAIL_LONGEST && began.is_none_or(|began| began < woken + WAKE_TAIL) {
            return;
        }
        // A long stretch of words after a wake is the TV or a conversation, not a request: no reply
        // over it.
        if text.split_whitespace().count() > LONGEST_REQUEST_WORDS {
            info!(text, "too long for a request; no reply");
            self.unhandled(Reason::NotUnderstood, text, None);
            return;
        }
        // "I'm not a fan of the music video.": the TV or the room after a false wake. A reply over
        // it is worse than none; a real request said wrong is said again. Whether the name was in
        // the wake word tells best. It is no gate for requests: it misses the name in about one
        // real wake in twenty.
        let addressed = match self.wake_named.take() {
            Some(named) => named,
            None => sounds_like_a_request(text),
        };
        if !addressed {
            info!(text, "not a request; no reply");
            self.unhandled(Reason::NotUnderstood, text, None);
            return;
        }
        info!(text, "not understood after the wake word");
        let answer = if lang == Lang::Norwegian {
            "Beklager, det fikk jeg ikke med meg."
        } else {
            "Sorry, I didn't catch that."
        };
        self.unhandled(Reason::NotUnderstood, text, Some(answer));
        self.turn = Some(self.turn_for(text, Some(answer.to_owned()), Outcome::NotUnderstood, start));
        self.busy.store(true, Relaxed);
        self.emit(AssistantEvent::Heard { text: text.to_owned() });
        // The music stays paused for a second try; it plays on if none comes.
        let hushed = self.hushed.take();
        self.say(answer.to_owned(), lang, start);
        self.hushed = hushed.map(|h| Hushed { last_heard: Instant::now(), ..h });
    }

    /// Whom the next answers address, by how the wake word's voice sounded. Follow-ups keep it.
    pub(super) fn address(&mut self, gender: Option<Gender>) {
        let (Some(persona), Some(female)) = (&mut self.persona, &self.config.honorific_female) else { return };
        if self.config.voice_gender.is_none() {
            return;
        }
        let male = self.config.honorific.as_deref().unwrap_or_default();
        persona.address(match gender {
            Some(Gender::Male) => male,
            Some(Gender::Female) => female,
            None => "",
        });
    }
}
