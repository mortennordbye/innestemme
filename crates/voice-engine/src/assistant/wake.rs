//! Wake words: whether the name was in them, and the reply when what follows is not understood.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use voice_assistant::dialog::WakeWord;
use voice_assistant::lang::Lang;
use voice_assistant::voice_gender::Gender;
use voice_assistant::whisper::Transcribe;
use voice_proto::SAMPLE_RATE;

use crate::activity::Outcome;
use crate::unhandled::Reason;

use super::follow_up::sounds_like_a_request;
use super::hush::Hushed;
use super::{AssistantEvent, Job, Worker, FOLLOW_UP};

/// A short utterance that starts this soon after a device's wake word holds the end of the wake
/// word ("Oh", "What?"), not the request. A longer one is the name and the request in one breath.
const WAKE_TAIL: Duration = Duration::from_millis(500);
const WAKE_TAIL_LONGEST: Duration = Duration::from_millis(1600);

/// Unrecognised speech after a wake longer than this many words gets no "didn't catch that".
const LONGEST_REQUEST_WORDS: usize = 12;

/// The longest the worker waits for a name check it needs: a check that hangs must not hold up the
/// conversation. One takes about 0.25 s at six threads on the cluster.
const NAME_WAIT: Duration = Duration::from_secs(3);

/// What a wake word tells of the assistant's name being said.
pub(super) enum WakeName {
    /// The device's own wake word: no audio.
    Unknown,
    /// The engine's, with its audio (24 kHz) to check.
    Check(Vec<i16>),
    /// A near miss already checked: the name was in it.
    Heard,
}

/// Where a name check's result goes.
pub(super) enum NameReply {
    /// The worker's own wake: it takes the result when a reply or the music needs it.
    Wake(mpsc::Sender<Option<bool>>),
    /// A satellite's near miss of the wake word.
    Satellite(tokio::sync::oneshot::Sender<Option<bool>>),
}

/// The name check with a model of its own, on a thread of its own. On the worker thread it held up
/// the request's draft by about a quarter of a second (Loki, Nov 2026: p50 248 ms, p90 315 ms).
pub(super) struct NameChecker {
    checks: mpsc::Sender<(Vec<i16>, NameReply)>,
}

impl NameChecker {
    /// `jobs` gets a [`Job::NameChecked`] after each wake's result, so the worker pauses the music
    /// without waiting for the request.
    pub(super) fn spawn(
        mut listener: Box<dyn Transcribe>,
        wake: WakeWord,
        jobs: mpsc::Sender<Job>,
    ) -> std::io::Result<Self> {
        let (checks, received) = mpsc::channel::<(Vec<i16>, NameReply)>();
        std::thread::Builder::new().name("name-check".into()).spawn(move || {
            for (audio, reply) in received {
                let named = name_in(listener.as_mut(), &wake, &audio);
                match reply {
                    NameReply::Wake(result) => {
                        let _ = result.send(named);
                        let _ = jobs.send(Job::NameChecked);
                    }
                    NameReply::Satellite(result) => {
                        let _ = result.send(named);
                    }
                }
            }
        })?;
        Ok(Self { checks })
    }

    pub(super) fn check(&self, audio: Vec<i16>, reply: NameReply) {
        if self.checks.send((audio, reply)).is_err() {
            warn!("the name check thread has stopped");
        }
    }
}

/// Whether a wake word's audio holds the assistant's name; `None` when it could not be transcribed.
fn name_in(transcriber: &mut dyn Transcribe, wake: &WakeWord, audio: &[i16]) -> Option<bool> {
    let start = Instant::now();
    let heard = match transcriber.transcribe(audio) {
        Ok(candidates) => candidates.into_iter().next().map(|c| c.text).unwrap_or_default(),
        Err(error) => {
            warn!(%error, "could not transcribe the wake word");
            return None;
        }
    };
    let named = wake.in_wake_word(&heard);
    info!(heard, named, took = ?start.elapsed(), "wake word check");
    Some(named)
}

impl Worker {
    /// A satellite woke, at `at`: listen for the request.
    pub(super) fn woken(&mut self, at: Instant, wake: WakeName) {
        info!("device wake word");
        self.woken_at = Some(at);
        // A new request: an earlier draft's utterance, if its end was lost, is over.
        self.answered_draft = None;
        // Until the wake word's voice is told (`Job::Voice`), which follows at once.
        self.address(None);
        self.follow_up_until = Some(Instant::now() + FOLLOW_UP);
        self.emit(AssistantEvent::Listening);
        // A check still out from an earlier wake is for a run that is over.
        self.wake_check = None;
        self.wake_named = None;
        match wake {
            WakeName::Unknown => self.hush_if_named(),
            // The music waits for the check's result.
            WakeName::Check(audio) => self.check_wake(audio),
            WakeName::Heard => {
                self.wake_named = Some(true);
                self.hush_if_named();
            }
        }
    }

    /// A wake word heard by the engine: its name check starts. Checked while the request is still
    /// being said: done when a reply needs it, it held up "didn't catch that" by a quarter of a
    /// second on the cluster.
    fn check_wake(&mut self, audio: Vec<i16>) {
        match &self.name_checker {
            Some(checker) => {
                let (result, check) = mpsc::channel();
                checker.check(audio, NameReply::Wake(result));
                self.wake_check = Some(check);
            }
            None => {
                self.wake_named = name_in(self.transcriber.as_mut(), &self.config.wake, &audio);
                self.hush_if_named();
            }
        }
    }

    /// A satellite's near miss: answered from the name check's thread when there is one.
    pub(super) fn check_name(&mut self, audio: Vec<i16>, reply: tokio::sync::oneshot::Sender<Option<bool>>) {
        match &self.name_checker {
            Some(checker) => checker.check(audio, NameReply::Satellite(reply)),
            None => {
                let _ = reply.send(name_in(self.transcriber.as_mut(), &self.config.wake, &audio));
            }
        }
    }

    /// Takes the wake's name check into `wake_named` when it is done, or with `wait`, waits for it:
    /// the reply to the request and the music need it.
    pub(super) fn take_wake_name(&mut self, wait: bool) {
        let Some(check) = &self.wake_check else { return };
        let named = match check.try_recv() {
            Ok(named) => named,
            Err(TryRecvError::Empty) if !wait => return,
            Err(TryRecvError::Empty) => {
                let start = Instant::now();
                let named = match check.recv_timeout(NAME_WAIT) {
                    Ok(named) => named,
                    Err(RecvTimeoutError::Timeout) => {
                        warn!("the wake word check took too long");
                        None
                    }
                    Err(RecvTimeoutError::Disconnected) => None,
                };
                info!(waited = ?start.elapsed(), "waited for the wake word check");
                named
            }
            Err(TryRecvError::Disconnected) => None,
        };
        self.wake_check = None;
        self.wake_named = named;
        self.hush_if_named();
    }

    /// Most wakes without the name are the TV or the room (Loki, 24-31 Oct: about 48 of 58), and
    /// pausing paused the TV under them. A real one still acts, over the music.
    fn hush_if_named(&mut self) {
        if self.wake_named == Some(false) {
            info!("no name in the wake word: the music plays on");
        } else {
            self.hush();
        }
    }

    /// A device woke on its wake word and the request was not understood: say so, once, rather
    /// than end the run in silence, which sounds as if the device never woke.
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::sync::broadcast;
    use voice_assistant::whisper::Transcript;

    use super::*;

    /// How long the fake name check takes: longer than any step the tests time.
    const CHECK: Duration = Duration::from_millis(400);

    /// The name check's model: slow, and it hears `text`. Notes when it is done.
    struct SlowName {
        text: &'static str,
        done: Arc<Mutex<Option<Instant>>>,
    }

    impl Transcribe for SlowName {
        fn transcribe(&mut self, _: &[i16]) -> anyhow::Result<Vec<Transcript>> {
            std::thread::sleep(CHECK);
            *self.done.lock().unwrap() = Some(Instant::now());
            Ok(vec![Transcript { lang: Lang::English, text: self.text.into(), score: 0.0 }])
        }
    }

    /// The request's listener: fast, and it hears a request no rule knows. Notes when it is done.
    struct Ear {
        done: Arc<Mutex<Option<Instant>>>,
    }

    impl Transcribe for Ear {
        fn transcribe(&mut self, _: &[i16]) -> anyhow::Result<Vec<Transcript>> {
            *self.done.lock().unwrap() = Some(Instant::now());
            Ok(vec![Transcript { lang: Lang::English, text: "florp the wibble".into(), score: 0.0 }])
        }
    }

    struct Woken {
        worker: Worker,
        events: broadcast::Receiver<AssistantEvent>,
        _reply: rtrb::Consumer<i16>,
        checked: Arc<Mutex<Option<Instant>>>,
        transcribed: Arc<Mutex<Option<Instant>>>,
    }

    /// A test worker whose name check hears `heard`, on the check's own thread.
    fn woken(heard: &'static str) -> Woken {
        let (mut worker, events, reply) = Worker::for_test(None);
        let (checked, transcribed) = (Arc::default(), Arc::default());
        let (jobs, _) = mpsc::channel();
        let name = SlowName { text: heard, done: Arc::clone(&checked) };
        worker.name_checker = Some(NameChecker::spawn(Box::new(name), worker.config.wake.clone(), jobs).unwrap());
        worker.transcriber = Box::new(Ear { done: Arc::clone(&transcribed) });
        Woken { worker, events, _reply: reply, checked, transcribed }
    }

    impl Woken {
        fn answers(&mut self) -> Vec<String> {
            let mut answers = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                if let AssistantEvent::Answer { text } = event {
                    answers.push(text);
                }
            }
            answers
        }
    }

    /// A second of the wake word's audio.
    fn wake_audio() -> WakeName {
        WakeName::Check(vec![0; SAMPLE_RATE as usize])
    }

    /// Two seconds of speech: a request, not the end of the wake word.
    fn request() -> Vec<i16> {
        vec![0; 2 * SAMPLE_RATE as usize]
    }

    #[test]
    fn a_draft_is_transcribed_while_the_name_is_checked() {
        let mut woken = woken("hey jarvis");
        let start = Instant::now();
        woken.worker.woken(start, wake_audio());
        assert!(start.elapsed() < CHECK / 4, "the wake waited {:?} for its check", start.elapsed());
        woken.worker.transcribe_draft(request());
        let transcribed = woken.transcribed.lock().unwrap().expect("the draft was transcribed");
        assert!(transcribed - start < CHECK / 4, "the draft waited {:?}", transcribed - start);
        assert!(woken.checked.lock().unwrap().is_none(), "the check was done before the draft");
        assert!(woken.worker.wake_check.is_some());
    }

    #[test]
    fn an_unknown_request_after_the_name_waits_for_the_check_and_is_answered() {
        let mut woken = woken("hey jarvis");
        woken.worker.woken(Instant::now() - Duration::from_secs(3), wake_audio());
        woken.worker.utterance(&request(), Instant::now());
        assert!(woken.checked.lock().unwrap().is_some());
        assert_eq!(woken.answers(), ["Sorry, I didn't catch that."]);
        assert!(woken.worker.wake_check.is_none());
    }

    #[test]
    fn an_unknown_request_after_a_wake_without_the_name_gets_no_reply() {
        let mut woken = woken("hey travis");
        woken.worker.woken(Instant::now() - Duration::from_secs(3), wake_audio());
        woken.worker.utterance(&request(), Instant::now());
        assert!(woken.checked.lock().unwrap().is_some());
        assert!(woken.answers().is_empty());
    }

    #[test]
    fn a_check_is_taken_once_it_is_done() {
        let mut woken = woken("hey jarvis");
        woken.worker.woken(Instant::now(), wake_audio());
        woken.worker.take_wake_name(false);
        assert_eq!(woken.worker.wake_named, None, "taken before it was done");
        std::thread::sleep(CHECK + Duration::from_millis(100));
        woken.worker.take_wake_name(false);
        assert_eq!(woken.worker.wake_named, Some(true));
        assert!(woken.worker.wake_check.is_none());
    }

    #[test]
    fn a_new_wake_drops_the_last_ones_check() {
        let mut woken = woken("hey travis");
        woken.worker.woken(Instant::now(), wake_audio());
        woken.worker.woken(Instant::now(), WakeName::Heard);
        std::thread::sleep(CHECK + Duration::from_millis(100));
        woken.worker.take_wake_name(true);
        assert_eq!(woken.worker.wake_named, Some(true));
    }

    #[test]
    fn a_near_miss_is_checked_off_the_worker_thread() {
        let mut woken = woken("hey jarvis");
        let (reply, answer) = tokio::sync::oneshot::channel();
        let start = Instant::now();
        woken.worker.check_name(vec![0; SAMPLE_RATE as usize], reply);
        assert!(start.elapsed() < CHECK / 4);
        assert_eq!(answer.blocking_recv(), Ok(Some(true)));
    }
}
