//! The handle satellites and the web page talk to the assistant through.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use tokio::sync::broadcast;
use tracing::{info, warn};
use voice_assistant::voice_gender::{self, VoiceGender};

use crate::activity::Activity;
use crate::party::Party;
use crate::screen::Screen;

use super::pause::Pause;
use super::wake::WakeName;
use super::{AssistantEvent, Job, Preview};

/// The end of the audio before the wake word fired that holds the wake word: 1.5 s at 16 kHz. More
/// takes in what was said before it, the TV or someone else.
const VOICE_AUDIO: usize = 24_000;

/// Talks to the assistant from outside the audio path (a satellite bridge).
#[derive(Clone)]
pub struct AssistantHandle {
    pub(super) jobs: mpsc::Sender<Job>,
    pub(super) events: broadcast::Sender<AssistantEvent>,
    pub(super) hear_now: Arc<AtomicBool>,
    pub(super) interrupted: Arc<AtomicBool>,
    pub pause: Arc<Pause>,
    pub screen: Arc<Screen>,
    pub party: Party,
    pub activity: Arc<Activity>,
    pub(super) voice_gender: Option<Arc<VoiceGender>>,
}

impl AssistantHandle {
    /// The wake word's audio (16 kHz), to address the speaker by how their voice sounds. Runs on a
    /// thread of its own while the request is still being said; the worker has the result long
    /// before the answer.
    pub fn voice_of(&self, wake: &[i16]) {
        let Some(model) = self.voice_gender.clone() else { return };
        let audio = wake[wake.len().saturating_sub(VOICE_AUDIO)..].to_vec();
        let jobs = self.jobs.clone();
        let spawned = std::thread::Builder::new().name("voice-gender".into()).spawn(move || {
            let start = Instant::now();
            match model.female(&audio) {
                Ok(female) => {
                    let gender = voice_gender::sure(female);
                    info!(?gender, female = format!("{female:.2}"), took = ?start.elapsed(), "voice");
                    let _ = jobs.send(Job::Voice(gender));
                }
                Err(error) => warn!(error = format!("{error:#}"), "could not tell the voice"),
            }
        });
        if let Err(error) = spawned {
            warn!(%error, "could not start the voice thread");
        }
    }

    /// The device heard its own wake word, or listens again after a question: the next utterance
    /// is the request. The device has finished playing, so there is no echo to wait out.
    pub fn woke(&self) {
        self.hear_now.store(true, Relaxed);
        let _ = self.jobs.send(Job::Woken(Instant::now(), WakeName::Unknown));
    }

    /// A wake word heard in the satellite's stream, with its audio (24 kHz), to check for the name
    /// if what follows is not understood.
    pub fn woke_on(&self, wake: Vec<i16>) {
        self.hear_now.store(true, Relaxed);
        let _ = self.jobs.send(Job::Woken(Instant::now(), WakeName::Check(wake)));
    }

    /// A near miss of the wake word whose name check found the name: a wake word.
    pub fn woke_named(&self) {
        self.hear_now.store(true, Relaxed);
        let _ = self.jobs.send(Job::Woken(Instant::now(), WakeName::Heard));
    }

    /// Whether the assistant's name is in `audio` (24 kHz); `None` when it could not be transcribed.
    pub fn check_name(&self, audio: Vec<i16>) -> tokio::sync::oneshot::Receiver<Option<bool>> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.jobs.send(Job::CheckName(audio, reply));
        answer
    }

    /// Nothing was asked after the satellite's wake word, likely the TV: the music plays on now
    /// rather than once the room has been quiet a while.
    pub fn gave_up(&self) {
        let _ = self.jobs.send(Job::NothingAsked);
    }

    /// The wake word was said over the answer: stop speaking it after the sentence being
    /// synthesized.
    pub fn interrupt(&self) {
        self.interrupted.store(true, Relaxed);
    }

    /// A satellite listens for a follow-up after an answer has played: hear the microphone at once,
    /// without the wake word's "not understood" reply to whatever is said.
    pub fn listen(&self) {
        self.hear_now.store(true, Relaxed);
    }

    /// The spoken answer to `text`, for the web page, when answering it changes nothing in the
    /// house. Nothing is played or sent to a satellite.
    pub fn preview(&self, text: String) -> tokio::sync::oneshot::Receiver<Preview> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.jobs.send(Job::Preview(text, reply));
        answer
    }

    /// What a typed request would do to each light, for the hub: nothing is switched.
    pub fn plan(&self, text: String) -> tokio::sync::oneshot::Receiver<serde_json::Value> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.jobs.send(Job::Plan(text, reply));
        answer
    }

    /// A web page button's request, done without a word; its answer's text.
    pub fn act(&self, text: String) -> tokio::sync::oneshot::Receiver<String> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.jobs.send(Job::Act(text, reply));
        answer
    }

    /// A satellite connected: its names, to look up its room in Home Assistant.
    pub fn satellite(&self, names: Vec<String>) {
        let _ = self.jobs.send(Job::Satellite(names));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AssistantEvent> {
        self.events.subscribe()
    }
}
