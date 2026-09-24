//! The `assistant` processor. The model thread listens and plays the reply; a worker thread
//! handles requests (transcription for Whisper, intent, tool, speech).
//!
//! Two listeners:
//! - Whisper (English and Norwegian): an energy detector cuts utterances, the worker transcribes
//!   each one with NB-Whisper and acts on those that start with the name.
//! - Kyutai (English only): streaming STT on the model thread, wake word and end of turn from the
//!   running transcript.
//!
//! Half duplex: while a request is being handled or its reply plays, and for a second after, the
//! microphone is ignored so the assistant does not answer itself through a laptop speaker.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tracing::{info, warn};
use voice_assistant::dialog::{Action, Dialog, WakeWord};
use voice_assistant::ha::{self, HomeAssistant, Target};
use voice_assistant::intent::{self, Intent};
use voice_assistant::jokes::Jokes;
use voice_assistant::lang::Lang;
use voice_assistant::llm::{self, Decision, Llm, Turn};
use voice_assistant::music::Player;
use voice_assistant::stt::SpeechToText;
use voice_assistant::timer::{self, Notice, Timers};
use voice_assistant::tts::{self, Tts};
use voice_assistant::vad::Vad;
use voice_assistant::weather::Weather;
use voice_assistant::whisper::Transcriber;
use voice_proto::{FRAME_SAMPLES, SAMPLE_RATE};
use voice_rt::noalloc::permit_alloc;

use crate::engine::{FrameProcessor, ProcessError};
use crate::metrics::Metrics;

/// Covers the STT's 0.5 s delay plus the room's echo tail.
const DEAF_AFTER_REPLY_FRAMES: u64 = 13;
/// The rolling KV cache is fine for long sessions, but position ids keep growing; start over
/// between requests once this many steps have passed (about 13 minutes).
const STT_RESET_STEPS: usize = 10_000;
/// "Them" and "that" refer to the last lights for this long.
const MEMORY: Duration = Duration::from_secs(120);
/// Earlier exchanges the language model sees, for follow-ups ("and tomorrow?").
const HISTORY_TURNS: usize = 4;
const HISTORY_MAX_AGE: Duration = Duration::from_secs(300);
/// After a bare name or an answer, speech within this time needs no name.
const FOLLOW_UP: Duration = Duration::from_secs(8);

pub struct AssistantConfig {
    /// Place used when a weather question names none.
    pub home: Option<String>,
    /// Home Assistant base URL and long-lived access token, for lights.
    pub home_assistant: Option<(String, String)>,
    /// The assistant's name, which wakes it.
    pub wake: WakeWord,
    /// Language model for requests the rules do not recognise.
    pub llm: Option<Llm>,
    /// Music Assistant player that music plays on: its name ("Living Room") or entity id.
    pub speaker: Option<String>,
    /// The room the microphone is in, for "turn off the lights" without a room. Default for a
    /// satellite: its area in Home Assistant.
    pub room: Option<String>,
    /// Directory for a wav of every utterance (debugging recognition).
    pub dump_utterances: Option<std::path::PathBuf>,
}

pub enum Listener {
    Whisper(Box<Transcriber>),
    Kyutai(Box<SpeechToText>),
}

enum Ear {
    Vad(Vad),
    Kyutai { stt: Box<SpeechToText>, dialog: Dialog },
}

/// What the assistant is doing, for satellites that show it (listening lights, speech URLs).
#[derive(Debug, Clone)]
pub enum AssistantEvent {
    /// The name was heard, or a device woke on its own wake word: listening for the request.
    Listening,
    /// A request is being handled.
    Heard { text: String },
    /// The answer's text, before its audio.
    Answer { text: String },
    /// Answer audio, 24 kHz mono, as it is synthesized.
    Speech(Arc<[i16]>),
    /// All of the answer's audio has been sent.
    Spoken,
    /// Nothing to say: "never mind", or nothing asked after the name.
    Done,
    /// A timer started, changed, was cancelled or ran out, for devices that show timers.
    Timer(Notice),
    /// Speech outside a conversation (a timer ran out, a reminder), 24 kHz mono, chime included.
    /// `timer_ring`: a plain timer, which devices with their own timer display ring for instead.
    Announcement { text: String, audio: Arc<[i16]>, timer_ring: bool },
}

/// Talks to the assistant from outside the audio path (a satellite bridge).
#[derive(Clone)]
pub struct AssistantHandle {
    jobs: mpsc::Sender<Job>,
    events: broadcast::Sender<AssistantEvent>,
    hear_now: Arc<AtomicBool>,
}

impl AssistantHandle {
    /// The device heard its own wake word, or listens again after a question: the next utterance
    /// is the request. The device has finished playing, so there is no echo to wait out.
    pub fn woke(&self) {
        self.hear_now.store(true, Relaxed);
        let _ = self.jobs.send(Job::Woken);
    }

    /// A satellite connected: its names, to look up its room in Home Assistant.
    pub fn satellite(&self, names: Vec<String>) {
        let _ = self.jobs.send(Job::Satellite(names));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AssistantEvent> {
        self.events.subscribe()
    }
}

enum Job {
    /// A session started: tell the speaker they can talk.
    Ready,
    /// A satellite heard its wake word itself.
    Woken,
    /// Kyutai path: already transcribed, English.
    Action(Action),
    /// Whisper path: audio still to transcribe.
    Utterance(Vec<i16>),
    /// A satellite connected, by its names.
    Satellite(Vec<String>),
}

pub struct AssistantProcessor {
    ear: Ear,
    jobs: mpsc::Sender<Job>,
    events: broadcast::Sender<AssistantEvent>,
    reply: rtrb::Consumer<i16>,
    /// Set from dispatch until the reply is queued.
    busy: Arc<AtomicBool>,
    /// A satellite started listening: stop ignoring the microphone at once.
    hear_now: Arc<AtomicBool>,
    was_busy: bool,
    frame: u64,
    deaf_until: u64,
}

impl AssistantProcessor {
    pub fn new(listener: Listener, tts: Box<dyn Tts>, config: AssistantConfig) -> anyhow::Result<Self> {
        let (ear, transcriber) = match listener {
            Listener::Whisper(transcriber) => (Ear::Vad(Vad::default()), Some(transcriber)),
            Listener::Kyutai(mut stt) => {
                // The first steps compile GPU kernels or fault in the weights.
                let start = Instant::now();
                for _ in 0..4 {
                    stt.step(&[0i16; FRAME_SAMPLES])?;
                }
                stt.reset()?;
                info!(elapsed = ?start.elapsed(), "stt warmed up");
                (Ear::Kyutai { stt, dialog: Dialog::new(config.wake.clone()) }, None)
            }
        };
        let (jobs, rx) = mpsc::channel();
        let (reply_tx, reply) = rtrb::RingBuffer::new(SAMPLE_RATE as usize * 60);
        let busy = Arc::new(AtomicBool::new(false));
        let speaker = config.speaker.clone();
        let (events, _) = broadcast::channel(256);
        let mut worker = Worker {
            events: events.clone(),
            reply: reply_tx,
            busy: busy.clone(),
            tts,
            config,
            weather: Weather::default(),
            jokes: Jokes::default(),
            home_assistant: None,
            player: Player::new(speaker),
            transcriber,
            follow_up_until: None,
            history: VecDeque::new(),
            last_lights: None,
            pending_lights: None,
            timers: Timers::default(),
            room: None,
        };
        worker.room = worker.config.room.clone();
        if let Some((url, token)) = &worker.config.home_assistant {
            let mut ha = HomeAssistant::new(url, token);
            match ha.check() {
                Ok(lights) => info!(url, lights, "home assistant connected"),
                Err(error) => {
                    warn!(url, %error, "home assistant not usable; light requests will fail")
                }
            }
            match worker.player.check(&ha) {
                Ok(player) => info!(player, "music assistant connected"),
                Err(error) => warn!(%error, "music assistant not usable; music requests will fail"),
            }
            worker.home_assistant = Some(ha);
        }
        if let Some(llm) = &worker.config.llm {
            let start = Instant::now();
            match llm.warm_up(&llm::system_prompt(
                worker.config.wake.name(),
                worker.config.home.as_deref(),
                worker.room.as_deref(),
                &today(),
            )) {
                Ok(()) => {
                    info!(model = llm.model(), elapsed = ?start.elapsed(), "language model warmed up")
                }
                Err(error) => {
                    warn!(%error, "language model not reachable; unrecognised requests get the fallback")
                }
            }
        }
        if let Some(t) = &mut worker.transcriber {
            let start = Instant::now();
            t.transcribe(&[0i16; FRAME_SAMPLES * 12])?;
            info!(elapsed = ?start.elapsed(), "whisper warmed up");
        }
        std::thread::Builder::new().name("assistant".into()).spawn(move || worker.run(rx))?;
        let hear_now = Arc::new(AtomicBool::new(false));
        Ok(Self { ear, jobs, events, reply, busy, hear_now, was_busy: false, frame: 0, deaf_until: 0 })
    }

    pub fn handle(&self) -> AssistantHandle {
        AssistantHandle { jobs: self.jobs.clone(), events: self.events.clone(), hear_now: self.hear_now.clone() }
    }
}

impl FrameProcessor for AssistantProcessor {
    fn process(&mut self, input: &[i16], output: &mut [i16], metrics: &Metrics) -> Result<(), ProcessError> {
        self.frame += 1;
        if self.hear_now.swap(false, Relaxed) {
            self.deaf_until = 0;
            self.was_busy = false;
            // A satellite plays answers itself; what is left in the ring is not heard anywhere.
            let queued = self.reply.slots();
            if let Ok(chunk) = self.reply.read_chunk(queued) {
                chunk.commit_all();
            }
        }
        let busy = self.busy.load(Relaxed);
        if busy || self.was_busy {
            let queued = (self.reply.slots() / FRAME_SAMPLES) as u64;
            self.deaf_until = self.deaf_until.max(self.frame + queued + DEAF_AFTER_REPLY_FRAMES);
        }
        self.was_busy = busy;
        let deaf = self.frame < self.deaf_until;

        let start = Instant::now();
        let result = permit_alloc(|| match &mut self.ear {
            Ear::Vad(vad) => {
                if deaf {
                    vad.reset();
                } else if let Some(utterance) = vad.push(input) {
                    info!(seconds = utterance.len() as f32 / SAMPLE_RATE as f32, "utterance");
                    let _ = self.jobs.send(Job::Utterance(utterance));
                }
                Ok(())
            }
            Ear::Kyutai { stt, dialog } => {
                let heard = stt.step(input).map_err(|error| warn!(%error, "stt step failed"))?;
                if !heard.words.is_empty() {
                    info!(words = heard.words.join(" "), deaf, "heard");
                }
                if deaf {
                    dialog.reset();
                    return Ok(());
                }
                for action in dialog.step(&heard.words, heard.pause) {
                    if matches!(action, Action::Request(_)) {
                        self.busy.store(true, Relaxed);
                    }
                    let _ = self.jobs.send(Job::Action(action));
                }
                if !dialog.is_listening() && stt.steps() > STT_RESET_STEPS {
                    let _ = stt.reset();
                }
                Ok(())
            }
        });
        metrics.engine_step.record(start.elapsed());

        let n = self.reply.slots().min(output.len());
        match self.reply.read_chunk(n) {
            Ok(chunk) => {
                let (a, b) = chunk.as_slices();
                output[..a.len()].copy_from_slice(a);
                output[a.len()..n].copy_from_slice(b);
                output[n..].fill(0);
                chunk.commit_all();
            }
            Err(_) => output.fill(0),
        }
        result.map_err(|()| ProcessError)
    }

    fn reset(&mut self) {
        match &mut self.ear {
            Ear::Vad(vad) => permit_alloc(|| vad.reset()),
            Ear::Kyutai { stt, dialog } => {
                permit_alloc(|| {
                    if let Err(error) = stt.reset() {
                        warn!(%error, "stt reset failed");
                    }
                });
                dialog.reset();
            }
        }
        self.deaf_until = 0;
        let queued = self.reply.slots();
        if let Ok(chunk) = self.reply.read_chunk(queued) {
            chunk.commit_all();
        }
        let _ = permit_alloc(|| self.jobs.send(Job::Ready));
    }
}

struct Worker {
    events: broadcast::Sender<AssistantEvent>,
    reply: rtrb::Producer<i16>,
    busy: Arc<AtomicBool>,
    tts: Box<dyn Tts>,
    config: AssistantConfig,
    weather: Weather,
    jokes: Jokes,
    home_assistant: Option<HomeAssistant>,
    player: Player,
    transcriber: Option<Box<Transcriber>>,
    /// Until then, speech counts without the name: after a bare name, and for a while after each
    /// answer, like a person who is still looking at you. Only recognised requests are acted on.
    follow_up_until: Option<Instant>,
    /// Recent exchanges, oldest first, for the language model.
    history: VecDeque<(Instant, Turn)>,
    /// The lights switched last, how, and when, for "turn them back on" and "reverse that".
    last_lights: Option<(Target, bool, Instant)>,
    /// "Turn off the lights" named no room and there was nothing to refer back to: the next
    /// answer is the room.
    pending_lights: Option<bool>,
    timers: Timers,
    /// Where the microphone is: the `room` setting, else the satellite's area.
    room: Option<String>,
}

impl Worker {
    fn run(mut self, jobs: mpsc::Receiver<Job>) {
        loop {
            // Wake up for the next timer as well as for work.
            let job = match self.timers.next_deadline() {
                Some(at) => match jobs.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(job) => job,
                    Err(RecvTimeoutError::Timeout) => {
                        self.ring_due_timers();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match jobs.recv() {
                    Ok(job) => job,
                    Err(_) => return,
                },
            };
            match job {
                Job::Action(Action::Wake) => {
                    info!("wake word");
                    self.emit(AssistantEvent::Listening);
                    self.play(&tts::wake_chime());
                }
                Job::Action(Action::GaveUp) => {
                    info!("nothing asked after the wake word");
                    self.emit(AssistantEvent::Done);
                    self.play(&tts::give_up_chime());
                }
                Job::Woken => {
                    info!("device wake word");
                    self.follow_up_until = Some(Instant::now() + FOLLOW_UP);
                    self.emit(AssistantEvent::Listening);
                }
                Job::Satellite(names) => self.satellite(&names),
                Job::Action(Action::Request(text)) => self.answer(&text, Lang::English, Instant::now()),
                Job::Utterance(audio) => self.utterance(&audio),
                Job::Ready => {
                    info!("ready");
                    (self.follow_up_until, self.last_lights, self.pending_lights) = (None, None, None);
                    self.history.clear();
                    self.play(&tts::ready_chime());
                }
            }
        }
    }

    fn utterance(&mut self, audio: &[i16]) {
        if let Some(dir) = &self.config.dump_utterances {
            if let Err(error) = dump(dir, audio) {
                warn!(%error, "could not save utterance");
            }
        }
        let Some(transcriber) = &mut self.transcriber else {
            return;
        };
        let start = Instant::now();
        let candidates = match transcriber.transcribe(audio) {
            Ok(candidates) if candidates.is_empty() => {
                info!(took = ?start.elapsed(), "not speech");
                return;
            }
            Ok(candidates) => candidates,
            Err(error) => {
                warn!(%error, "transcription failed");
                return;
            }
        };
        let follow_up = self.follow_up_until.is_some_and(|until| Instant::now() < until);
        // A translation usually drops the name, so a candidate that starts with the wake word is
        // the real transcript. Otherwise the most confident one.
        let heard = match candidates.iter().position(|c| self.config.wake.strip(&c.text).is_some()) {
            Some(i) if !follow_up => &candidates[i],
            _ => &candidates[0],
        };
        info!(text = heard.text, lang = ?heard.lang, took = ?start.elapsed(), "heard");
        let request = match self.config.wake.strip(&heard.text) {
            Some(rest) if rest.is_empty() => {
                info!("wake word");
                self.follow_up_until = Some(Instant::now() + FOLLOW_UP);
                self.emit(AssistantEvent::Listening);
                self.play(&tts::wake_chime());
                return;
            }
            Some(rest) => (rest, true),
            None if follow_up => (heard.text.clone(), false),
            None => {
                info!(text = heard.text, "not addressed");
                return;
            }
        };
        let (request, addressed) = request;
        // Without the name, only act on something recognisable: in the follow-up window the
        // microphone also hears the room.
        // With a language model, it decides whether unrecognised words were meant for the assistant.
        let unknown = intent::parse(&request) == Intent::Unknown;
        let pending = self.pending_lights.is_some() || self.timers.awaiting_duration();
        if !addressed && !pending && unknown && self.config.llm.is_none() {
            info!(text = heard.text, "not addressed");
            return;
        }
        self.busy.store(true, Relaxed);
        self.answer(&request, heard.lang, start);
    }

    fn answer(&mut self, text: &str, lang: Lang, start: Instant) {
        self.emit(AssistantEvent::Heard { text: text.to_owned() });
        let Some(answer) = self.compose(text, lang) else {
            info!(request = text, "conversation ended");
            self.emit(AssistantEvent::Done);
            self.follow_up_until = None;
            self.pending_lights = None;
            self.timers.clear_pending();
            self.busy.store(false, Relaxed);
            return;
        };
        info!(request = text, answer, ?lang, after = ?start.elapsed(), "answering");
        self.history.push_back((Instant::now(), Turn { user: text.to_owned(), assistant: answer.clone() }));
        while self.history.len() > HISTORY_TURNS {
            self.history.pop_front();
        }
        self.emit(AssistantEvent::Answer { text: answer.clone() });
        // Audio goes to the speaker as it is synthesized; `say` hands over whole sentences.
        let (reply, events, mut samples, mut first) = (&mut self.reply, &self.events, 0usize, None);
        for sentence in answer.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty()) {
            let result = self.tts.speak(sentence, lang, &mut |pcm| {
                first.get_or_insert_with(|| start.elapsed());
                samples += pcm.len();
                push(reply, pcm);
                let _ = events.send(AssistantEvent::Speech(pcm.into()));
            });
            if let Err(error) = result {
                warn!(%error, "speech synthesis failed");
            }
        }
        let seconds = samples as f32 / SAMPLE_RATE as f32;
        info!(first_audio_after = ?first, done_after = ?start.elapsed(), seconds, "spoken");
        self.emit(AssistantEvent::Spoken);
        // The reply is queued faster than it plays; the window opens when it has been heard.
        self.follow_up_until = Some(Instant::now() + Duration::from_secs_f32(seconds) + FOLLOW_UP);
        self.busy.store(false, Relaxed);
    }

    /// The spoken answer, or `None` to end the conversation quietly.
    fn compose(&mut self, text: &str, lang: Lang) -> Option<String> {
        let intent = intent::parse(text);
        // "Set a timer." "For how long?" "Ten minutes."
        if self.timers.awaiting_duration() {
            if let Some((answer, notices)) = self.timers.answer_pending(text, lang, Instant::now()) {
                self.notify(notices);
                return Some(answer);
            }
            self.timers.clear_pending();
        }
        if let Some(on) = self.pending_lights.take() {
            if intent == Intent::Unknown {
                return Some(self.lights(text, on, lang));
            }
        }
        if intent == Intent::Unknown {
            if let Some(llm) = &self.config.llm {
                let system = llm::system_prompt(
                    self.config.wake.name(),
                    self.config.home.as_deref(),
                    self.room.as_deref(),
                    &today(),
                );
                let history: Vec<Turn> = self
                    .history
                    .iter()
                    .filter(|(at, _)| at.elapsed() < HISTORY_MAX_AGE)
                    .map(|(_, turn)| turn.clone())
                    .collect();
                let start = Instant::now();
                match llm.decide(&system, &history, text, lang) {
                    Ok(Decision::Act(intent)) => {
                        info!(?intent, took = ?start.elapsed(), "language model chose a skill");
                        return self.act(intent, lang);
                    }
                    Ok(Decision::Say(answer)) => {
                        info!(took = ?start.elapsed(), "language model answered");
                        return Some(answer);
                    }
                    Ok(Decision::Ignore) => {
                        info!(request = text, took = ?start.elapsed(), "language model: not meant for me");
                        return None;
                    }
                    Err(error) => warn!(%error, "language model failed; falling back to the rules"),
                }
            }
        }
        self.act(intent, lang)
    }

    /// Runs a recognised request; the spoken answer, or `None` to end quietly.
    fn act(&mut self, intent: Intent, lang: Lang) -> Option<String> {
        let no = lang == Lang::Norwegian;
        Some(match intent {
            Intent::Weather { place, day } => {
                let Some(place) = place.or_else(|| self.config.home.clone()) else {
                    return Some(if no {
                        "Hvilket sted? Si for eksempel været i Oslo.".into()
                    } else {
                        "Which place? Say, for example, the weather in London.".into()
                    });
                };
                match self.weather.report(&place, day, lang) {
                    Ok(Some(report)) => report,
                    Ok(None) if no => {
                        format!("Beklager, jeg fant ikke noe sted som heter {place}.")
                    }
                    Ok(None) => format!("Sorry, I could not find a place called {place}."),
                    Err(error) => {
                        warn!(%error, place, "weather lookup failed");
                        if no {
                            "Beklager, jeg får ikke kontakt med værtjenesten.".into()
                        } else {
                            "Sorry, I could not reach the weather service.".into()
                        }
                    }
                }
            }
            Intent::Joke => self.jokes.tell(lang),
            Intent::Time => time_of_day(lang),
            Intent::Music(command) => self.player.run(self.home_assistant.as_ref(), &command, lang),
            Intent::Timer(command) => {
                let (answer, notices) = self.timers.run(command, lang, Instant::now());
                self.notify(notices);
                answer
            }
            Intent::Lights { on, target } => self.lights(&target, on, lang),
            Intent::Undo => match self.recent_lights() {
                Some((target, on)) => self.switch(&target, !on, lang, true),
                None if no => "Det er ingenting å angre.".into(),
                None => "There's nothing to undo.".into(),
            },
            Intent::Thanks if no => "Bare hyggelig.".into(),
            Intent::Thanks => "You're welcome.".into(),
            Intent::Cancel => return None,
            Intent::Unknown if no => "Beklager, foreløpig kan jeg bare været, vitser, lyset, musikk og timere.".into(),
            Intent::Unknown => "Sorry, so far I can only do the weather, jokes, the lights, music and timers.".into(),
        })
    }

    fn lights(&mut self, request: &str, on: bool, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let recent = self.recent_lights().map(|(target, _)| target);
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let found = match ha.find(request) {
            Ok(found) => found,
            Err(error) => return unreachable(&error, no),
        };
        // "Turn off the lights", "in here": this room. "Turn them off": the last lights.
        let here = self.room.as_deref().and_then(|room| ha.find(room).ok().flatten());
        let target = match found {
            Some(target) => Some(target),
            None if mentions_this_room(request) => here,
            None if !ha::names_something(request) => {
                if refers_back(request) {
                    recent.or(here)
                } else {
                    here.or(recent)
                }
            }
            None if no => return "Jeg fant ikke det lyset.".into(),
            None => return "I couldn't find that light.".into(),
        };
        let Some(target) = target else {
            self.pending_lights = Some(on);
            return if no { "Hvilket rom?" } else { "Which room?" }.into();
        };
        self.switch(&target, on, lang, false)
    }

    /// A satellite connected: unless the `room` setting says otherwise, its room is its area in
    /// Home Assistant.
    fn satellite(&mut self, names: &[String]) {
        if self.config.room.is_some() {
            return;
        }
        let Some(ha) = &self.home_assistant else { return };
        let names: Vec<&str> = names.iter().map(String::as_str).filter(|n| !n.is_empty()).collect();
        match ha.device_area(&names) {
            Ok(Some(area)) => {
                info!(room = area, "satellite room from home assistant");
                self.room = Some(area);
            }
            Ok(None) => info!(?names, "the satellite has no area in home assistant; set `room`"),
            Err(error) => warn!(%error, "could not look up the satellite's room"),
        }
    }

    fn recent_lights(&self) -> Option<(Target, bool)> {
        self.last_lights.as_ref().filter(|(.., at)| at.elapsed() < MEMORY).map(|(target, on, _)| (target.clone(), *on))
    }

    fn switch(&mut self, target: &Target, on: bool, lang: Lang, undo: bool) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return "Home Assistant isn't connected yet.".into();
        };
        if let Err(error) = ha.set(target, on) {
            return unreachable(&error, no);
        }
        info!(label = target.label, ids = ?target.ids, on, undo, "lights switched");
        self.last_lights = Some((target.clone(), on, Instant::now()));
        let label = target.label.to_lowercase();
        let state = match (no, on, undo) {
            (false, true, false) => "on",
            (false, false, false) => "off",
            (false, true, true) => "back on",
            (false, false, true) => "back off",
            (true, true, _) => "på",
            (true, false, _) => "av",
        };
        match (no, target.area) {
            (false, true) => format!("Okay, the {label} lights are {state}."),
            (false, false) => format!("Okay, the {label} is {state}."),
            (true, true) => format!("Ok, lyset i {label} er {state}."),
            (true, false) => format!("Ok, {label} er {state}."),
        }
    }

    fn notify(&self, notices: Vec<Notice>) {
        for notice in notices {
            info!(kind = ?notice.kind, id = notice.id, left = notice.seconds_left, "timer");
            self.emit(AssistantEvent::Timer(notice));
        }
    }

    /// Rings for timers that ran out and speaks what they were for. The audio also goes to
    /// satellites as an announcement.
    fn ring_due_timers(&mut self) {
        for (timer, notice) in self.timers.due(Instant::now()) {
            let text = timer::finished_text(&timer);
            info!(text, "timer done");
            self.emit(AssistantEvent::Timer(notice));
            // Deaf while it plays, like an answer.
            self.busy.store(true, Relaxed);
            let mut audio = if timer.is_reminder { tts::reminder_chime() } else { tts::alarm_chime() };
            if let Err(error) = self.tts.speak(&text, timer.lang, &mut |pcm| audio.extend_from_slice(pcm)) {
                warn!(%error, "speech synthesis failed");
            }
            push(&mut self.reply, &audio);
            self.busy.store(false, Relaxed);
            self.emit(AssistantEvent::Announcement { text, audio: audio.into(), timer_ring: !timer.is_reminder });
        }
    }

    fn emit(&self, event: AssistantEvent) {
        // No satellite listening is fine.
        let _ = self.events.send(event);
    }

    fn play(&mut self, pcm: &[i16]) {
        push(&mut self.reply, pcm);
    }
}

/// "In here", "this room", "her inne", "dette rommet".
fn mentions_this_room(request: &str) -> bool {
    let text = format!(" {} ", request.to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != ' ', " "));
    [" here ", " this room ", " her inne ", " her ", " dette rommet ", " rommet her "].iter().any(|p| text.contains(p))
}

/// "Them", "it", "those": the lights just switched rather than the room.
fn refers_back(request: &str) -> bool {
    request
        .split_whitespace()
        .map(voice_assistant::dialog::normalize)
        .any(|w| ["them", "it", "those", "these", "they", "that", "dem", "den", "det", "de"].contains(&w.as_str()))
}

/// Local date for the language model: "Thursday 24 September 2026".
fn today() -> String {
    jiff::Zoned::now().strftime("%A %-d %B %Y").to_string()
}

/// "It's 20:15." / "Klokka er 20:15."
fn time_of_day(lang: Lang) -> String {
    let now = jiff::Zoned::now();
    match lang {
        Lang::English => format!("It's {}.", now.strftime("%H:%M")),
        Lang::Norwegian => format!("Klokka er {}.", now.strftime("%H:%M")),
    }
}

fn unreachable(error: &anyhow::Error, no: bool) -> String {
    warn!(%error, "home assistant call failed");
    if no { "Beklager, jeg får ikke kontakt med Home Assistant." } else { "Sorry, I couldn't reach Home Assistant." }
        .into()
}

fn dump(dir: &std::path::Path, audio: &[i16]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
    let path = dir.join(format!("utterance-{stamp}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec)?;
    audio.iter().try_for_each(|&s| writer.write_sample(s))?;
    writer.finalize()?;
    info!(path = %path.display(), "utterance saved");
    Ok(())
}

fn push(reply: &mut rtrb::Producer<i16>, pcm: &[i16]) {
    let n = pcm.len().min(reply.slots());
    if let Ok(chunk) = reply.write_chunk_uninit(n) {
        chunk.fill_from_iter(pcm[..n].iter().copied());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_room_and_back_references() {
        assert!(mentions_this_room("turn off the lights in here"));
        assert!(mentions_this_room("turn on the light in this room"));
        assert!(mentions_this_room("skru av lyset her inne"));
        assert!(!mentions_this_room("turn off the lights"));
        assert!(!mentions_this_room("turn off the kitchen lights"));
        assert!(refers_back("turn them off"));
        assert!(refers_back("skru dem på igjen"));
        assert!(!refers_back("turn off the lights"));
    }
}
