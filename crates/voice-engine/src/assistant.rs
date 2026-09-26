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
use voice_assistant::geo::{self, Location};
use voice_assistant::ha::{self, HomeAssistant, Target};
use voice_assistant::intent::{self, Day, Intent, LightLevel};
use voice_assistant::jokes::Jokes;
use voice_assistant::lang::Lang;
use voice_assistant::llm::{self, Decision, Llm, Turn};
use voice_assistant::music::Player;
use voice_assistant::power::{self, Power};
use voice_assistant::shopping::{Change, ListCommand, ShoppingList};
use voice_assistant::stt::SpeechToText;
use voice_assistant::timer::{self, Notice, Timers};
use voice_assistant::transit::Transit;
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
/// How often the worker checks that the language model's prompt is still the one it warmed.
const WARM_CHECK: Duration = Duration::from_secs(600);
/// Earlier exchanges the language model sees, for follow-ups ("and tomorrow?").
const HISTORY_TURNS: usize = 4;
const HISTORY_MAX_AGE: Duration = Duration::from_secs(300);
/// After a bare name or an answer, speech within this time needs no name.
const FOLLOW_UP: Duration = Duration::from_secs(8);

pub struct AssistantConfig {
    /// Place used when a weather question names none; also the home's name in answers.
    pub home: Option<String>,
    /// The home's street address or coordinates: exact weather, and the stops near it.
    pub address: Option<String>,
    /// Stops for departures (names or NSR ids) instead of the ones nearest the address.
    pub transit_stops: Vec<String>,
    /// Contact (email or URL) for the User-Agent that MET Norway asks for.
    pub contact: Option<String>,
    /// Norwegian electricity price area (NO1-NO5); guessed from the address when unset.
    pub price_area: Option<String>,
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
        // MET Norway asks for an application name and a contact.
        let user_agent = match &config.contact {
            Some(contact) => format!("innestemme/{} {contact}", env!("CARGO_PKG_VERSION")),
            None => format!("innestemme/{}", env!("CARGO_PKG_VERSION")),
        };
        let home_location = config.address.as_deref().and_then(|address| {
            match geo::home(&geo::agent(&user_agent), address, config.home.as_deref()) {
                Ok(Some(home)) => {
                    info!(
                        label = home.label,
                        latitude = home.latitude,
                        longitude = home.longitude,
                        "home address found"
                    );
                    Some(home)
                }
                Ok(None) => {
                    warn!("the address was not found; weather and departures use `home` instead");
                    None
                }
                Err(error) => {
                    warn!(error = format!("{error:#}"), "could not look up the address");
                    None
                }
            }
        });
        let transit = Transit::new(&user_agent, home_location.clone(), config.transit_stops.clone());
        let in_norway =
            |home: &Location| (57.5..71.5).contains(&home.latitude) && (4.0..31.5).contains(&home.longitude);
        let price_area = config.price_area.clone().or_else(|| {
            home_location
                .as_ref()
                .filter(|home| in_norway(home))
                .map(|h| power::area_for(h.latitude, h.longitude).into())
        });
        if let Some(area) = &price_area {
            info!(area, "electricity price area");
        }
        let power = price_area.map(|area| Power::new(&user_agent, &area));
        let mut worker = Worker {
            events: events.clone(),
            reply: reply_tx,
            busy: busy.clone(),
            tts,
            config,
            weather: Weather::new(&user_agent),
            transit,
            power,
            home_location,
            jokes: Jokes::default(),
            home_assistant: None,
            player: Player::new(speaker),
            transcriber,
            follow_up_until: None,
            history: VecDeque::new(),
            last_lights: None,
            pending_lights: None,
            shopping: ShoppingList::default(),
            last_list: None,
            pending_list_add: false,
            pending_scene: None,
            timers: Timers::default(),
            room: None,
            warmed_prompt: String::new(),
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
            let system = worker.system_prompt();
            match llm.warm_up(&system) {
                Ok(()) => {
                    info!(model = llm.model(), elapsed = ?start.elapsed(), "language model warmed up");
                    worker.warmed_prompt = system;
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
    transit: Transit,
    /// Electricity prices; `None` outside Norway or without a price area.
    power: Option<Power>,
    /// The home from the `address` setting.
    home_location: Option<Location>,
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
    shopping: ShoppingList,
    /// The last change to the shopping list and when, for "undo".
    last_list: Option<(Change, Instant)>,
    /// "Add to the shopping list" named nothing: the next answer is the items.
    pending_list_add: bool,
    /// A scene request that exists in several rooms: the next answer is the room.
    pending_scene: Option<String>,
    timers: Timers,
    /// Where the microphone is: the `room` setting, else the satellite's area.
    room: Option<String>,
    /// The system prompt the language model last saw. It holds the date and the room; a new one
    /// costs seconds of prompt processing on a CPU, so it is sent ahead of the next question.
    warmed_prompt: String,
}

impl Worker {
    fn run(mut self, jobs: mpsc::Receiver<Job>) {
        loop {
            // Wake up for the next timer, and now and then to keep the language model warm.
            let next_timer = self.timers.next_deadline().map(|at| at.saturating_duration_since(Instant::now()));
            let wait = next_timer.map_or(WARM_CHECK, |t| t.min(WARM_CHECK));
            let job = match jobs.recv_timeout(wait) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => {
                    self.ring_due_timers();
                    self.keep_warm();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
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
        if std::mem::take(&mut self.pending_list_add) && intent == Intent::Unknown {
            // "Milk and eggs": the same splitting as a full request.
            if let Some(ListCommand::Add(items)) = voice_assistant::shopping::parse(&format!("add {text} to the list"))
            {
                return Some(self.shopping_list(ListCommand::Add(items), lang));
            }
        }
        if let Some(scene) = self.pending_scene.take() {
            if intent == Intent::Unknown {
                return Some(self.scene(&format!("{scene} in {text}"), lang));
            }
        }
        if intent == Intent::Unknown {
            // A script or scene said by its name alone: "movie time".
            if let Some(answer) = self.by_exact_name(text, lang) {
                return Some(answer);
            }
        }
        if intent == Intent::Unknown {
            if let Some(llm) = &self.config.llm {
                let system = self.system_prompt();
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
            Intent::Weather { place, day } => self.weather(place, day, lang),
            Intent::Transit(query) => {
                if !self.transit.is_configured() {
                    return Some(if no {
                        "Jeg vet ikke hvor du bor ennå. Sett address i innstillingene.".into()
                    } else {
                        "I don't know where home is yet. Set address in the settings.".into()
                    });
                }
                match self.transit.next(&query, lang) {
                    Ok(answer) => answer,
                    Err(error) => {
                        warn!(%error, "departures lookup failed");
                        if no { "Beklager, jeg får ikke kontakt med Entur." } else { "Sorry, I couldn't reach Entur." }
                            .into()
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
            Intent::LightsStatus { target } => self.lights_status(&target, lang),
            Intent::LightLevel { level, target } => self.light_level(&target, &level, lang),
            Intent::Power(query) => match &mut self.power {
                None if no => "Jeg vet ikke hvilket prisområde du er i. Sett price-area i innstillingene.".into(),
                None => "I don't know your electricity price area. Set price-area in the settings.".into(),
                Some(power) => match power.answer(query, lang) {
                    Ok(answer) => answer,
                    Err(error) => {
                        warn!(error = format!("{error:#}"), "electricity prices failed");
                        if no { "Beklager, jeg får ikke hentet strømprisene." } else { "Sorry, I couldn't get the electricity prices." }.into()
                    }
                },
            },
            Intent::WhosHome(name) => self.whos_home(name.as_deref(), lang),
            Intent::Briefing => self.briefing(lang),
            Intent::ShoppingList(command) => self.shopping_list(command, lang),
            Intent::Scene(request) => self.scene(&request, lang),
            Intent::Script(request) => self.script(&request, lang),
            Intent::Undo => self.undo(lang),
            Intent::Thanks if no => "Bare hyggelig.".into(),
            Intent::Thanks => "You're welcome.".into(),
            Intent::Cancel => return None,
            Intent::Unknown if no => {
                "Beklager, foreløpig kan jeg bare været, vitser, lyset, scener, musikk, timere, handlelista og avganger."
                    .into()
            }
            Intent::Unknown => {
                "Sorry, so far I can only do the weather, jokes, lights, scenes, music, timers, the shopping list and \
                 departures."
                    .into()
            }
        })
    }

    /// A named place, else the home: the address when it is set, else the `home` place name.
    fn weather(&mut self, place: Option<String>, day: Day, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let named = place.or_else(|| self.home_location.is_none().then(|| self.config.home.clone()).flatten());
        let location = match named {
            None => match &self.home_location {
                Some(home) => Ok(Some(home.clone())),
                None if no => return "Hvilket sted? Si for eksempel været i Oslo.".into(),
                None => return "Which place? Say, for example, the weather in London.".into(),
            },
            Some(name) => self.weather.find(&name),
        };
        let report = location.and_then(|location| match location {
            Some(location) => self.weather.report(&location, day, lang).map(Some),
            None => Ok(None),
        });
        match report {
            Ok(Some(report)) => report,
            Ok(None) if no => "Beklager, jeg fant ikke det stedet.".into(),
            Ok(None) => "Sorry, I couldn't find that place.".into(),
            Err(error) => {
                warn!(error = format!("{error:#}"), "weather lookup failed");
                if no {
                    "Beklager, jeg får ikke kontakt med værtjenesten.".into()
                } else {
                    "Sorry, I could not reach the weather service.".into()
                }
            }
        }
    }

    fn lights(&mut self, request: &str, on: bool, lang: Lang) -> String {
        match self.light_target(request, lang) {
            Ok(Some(target)) => self.switch(&target, on, lang, false),
            Ok(None) => {
                self.pending_lights = Some(on);
                if lang == Lang::Norwegian { "Hvilket rom?" } else { "Which room?" }.into()
            }
            Err(answer) => answer,
        }
    }

    /// The lights a request means: the ones it names; "in here" and no name at all mean this room;
    /// "them" the last lights. `Ok(None)`: no idea which room. `Err` is the answer to speak.
    fn light_target(&mut self, request: &str, lang: Lang) -> Result<Option<Target>, String> {
        let no = lang == Lang::Norwegian;
        let recent = self.recent_lights().map(|(target, _)| target);
        let Some(ha) = &mut self.home_assistant else {
            return Err(if no {
                "Home Assistant er ikke koblet til ennå."
            } else {
                "Home Assistant isn't connected yet."
            }
            .into());
        };
        let found = ha.find(request).map_err(|error| unreachable(&error, no))?;
        let here = self.room.as_deref().and_then(|room| ha.find(room).ok().flatten());
        match found {
            Some(target) => Ok(Some(target)),
            None if mentions_this_room(request) => Ok(here),
            None if !ha::names_something(request) => {
                Ok(if refers_back(request) { recent.or(here) } else { here.or(recent) })
            }
            None if no => Err("Jeg fant ikke det lyset.".into()),
            None => Err("I couldn't find that light.".into()),
        }
    }

    fn light_level(&mut self, request: &str, level: &LightLevel, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let target = match self.light_target(request, lang) {
            Ok(Some(target)) => target,
            Ok(None) if no => return "Si hvilket rom, for eksempel: demp lyset i stua.".into(),
            Ok(None) => return "Say which room, for example: dim the living room.".into(),
            Err(answer) => return answer,
        };
        let Some(ha) = &self.home_assistant else { return "Home Assistant isn't connected yet.".into() };
        if let Err(error) = ha.set_level(&target, level) {
            return unreachable(&error, no);
        }
        info!(label = target.label, ids = ?target.ids, ?level, "light level set");
        let label = target.label.to_lowercase();
        let lights = match (no, target.area) {
            (false, true) => format!("the {label} lights are"),
            (false, false) => format!("the {label} is"),
            (true, _) => format!("lyset i {label} er"),
        };
        match (no, level) {
            (false, LightLevel::Percent(0)) => format!("Okay, {lights} off."),
            (true, LightLevel::Percent(0)) => format!("Ok, {lights} av."),
            (false, LightLevel::Percent(n)) => format!("Okay, {lights} at {n} percent."),
            (true, LightLevel::Percent(n)) => format!("Ok, {lights} på {n} prosent."),
            (false, LightLevel::Brighter) => "Okay, brighter.".into(),
            (true, LightLevel::Brighter) => "Ok, lysere.".into(),
            (false, LightLevel::Dimmer) => "Okay, dimmed.".into(),
            (true, LightLevel::Dimmer) => "Ok, dempet.".into(),
            (false, LightLevel::Color(colour)) => format!("Okay, {lights} {colour}."),
            (true, LightLevel::Color(_)) => "Ok, fargen er endret.".into(),
            (false, LightLevel::Warm) => "Okay, warmer light.".into(),
            (true, LightLevel::Warm) => "Ok, varmere lys.".into(),
            (false, LightLevel::Cool) => "Okay, cooler light.".into(),
            (true, LightLevel::Cool) => "Ok, kaldere lys.".into(),
        }
    }

    /// "Who's home?": everyone and where they are; "is Ingrid home?": that person.
    fn whos_home(&mut self, name: Option<&str>, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let people = match ha.people() {
            Ok(people) => people,
            Err(error) => return unreachable(&error, no),
        };
        let first_name = |p: &ha::Person| p.name.split_whitespace().next().unwrap_or(&p.name).to_owned();
        let place = |p: &ha::Person| match (no, p.state.as_str()) {
            (false, "home") => "home".to_owned(),
            (true, "home") => "hjemme".to_owned(),
            (false, "not_home") => "out".to_owned(),
            (true, "not_home") => "ute".to_owned(),
            (false, "unknown" | "unavailable") => "somewhere I can't tell".to_owned(),
            (true, "unknown" | "unavailable") => "et sted jeg ikke vet".to_owned(),
            (false, zone) => format!("at {zone}"),
            (true, zone) => format!("på {zone}"),
        };
        if let Some(name) = name {
            let names = voice_assistant::names::Names::new(people.iter().map(first_name));
            let Some(person) = names.best(name).and_then(|found| people.iter().find(|p| first_name(p) == found.name))
            else {
                return if no { format!("Jeg vet ikke hvor {name} er.") } else { format!("I don't track {name}.") };
            };
            let who = first_name(person);
            return match (no, person.state == "home") {
                (false, true) => format!("Yes, {who} is home."),
                (true, true) => format!("Ja, {who} er hjemme."),
                (false, false) => format!("No, {who} is {}.", place(person)),
                (true, false) => format!("Nei, {who} er {}.", place(person)),
            };
        }
        let home: Vec<String> = people.iter().filter(|p| p.state == "home").map(first_name).collect();
        let and = if no { "og" } else { "and" };
        let join = |names: &[String]| match names {
            [] => String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} {and} {last}", rest.join(", ")),
        };
        let is = if no { "er" } else { "is" };
        let mut clauses: Vec<String> = Vec::new();
        match (no, home.len()) {
            (_, 0) => {}
            (false, n) => clauses.push(format!("{} {} home", join(&home), if n == 1 { "is" } else { "are" })),
            (true, _) => clauses.push(format!("{} er hjemme", join(&home))),
        }
        clauses.extend(
            people.iter().filter(|p| p.state != "home").map(|p| format!("{} {is} {}", first_name(p), place(p))),
        );
        match (no, home.is_empty()) {
            (false, true) if clauses.is_empty() => "Home Assistant tracks nobody.".into(),
            (true, true) if clauses.is_empty() => "Home Assistant følger ingen.".into(),
            (false, true) => format!("Nobody is home; {}.", clauses.join(", ")),
            (true, true) => format!("Ingen er hjemme; {}.", clauses.join(", ")),
            _ => format!("{}.", clauses.join("; ")),
        }
    }

    /// "Good morning": greeting and time, the weather at home, the shopping list, power prices.
    fn briefing(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let now = jiff::Zoned::now();
        let greeting = match (no, now.hour()) {
            (false, 4..=11) => "Good morning",
            (false, 12..=17) => "Good afternoon",
            (false, _) => "Good evening",
            (true, 4..=9) => "God morgen",
            (true, 10..=17) => "Hei",
            (true, _) => "God kveld",
        };
        let mut parts = vec![time_of_day(lang).replacen("It's", &format!("{greeting}! It's"), 1)];
        if no {
            parts[0] = format!("{greeting}! {}", time_of_day(lang));
        }
        if self.home_location.is_some() || self.config.home.is_some() {
            parts.push(self.weather(None, Day::Today, lang));
        }
        if let Some(ha) = &self.home_assistant {
            if let Ok((answer, _)) = self.shopping.run(ha, &ListCommand::Read, lang) {
                // Only a list with something on it is news.
                if !answer.contains("empty") && !answer.contains("tom") {
                    parts.push(answer);
                }
            }
        }
        if let Some(power) = &mut self.power {
            if let Ok(answer) = power.answer(power::PowerQuery::Now, lang) {
                parts.push(answer);
            }
        }
        parts.join(" ")
    }

    fn system_prompt(&self) -> String {
        llm::system_prompt(self.config.wake.name(), self.config.home.as_deref(), self.room.as_deref(), &today())
    }

    /// Sends a changed system prompt (new day, new room) to the language model in the background,
    /// so the next question does not wait for it.
    fn keep_warm(&mut self) {
        let Some(llm) = &self.config.llm else { return };
        let system = self.system_prompt();
        if system == self.warmed_prompt {
            return;
        }
        self.warmed_prompt = system.clone();
        let llm = llm.clone();
        let spawned = std::thread::Builder::new().name("llm-warm".into()).spawn(move || {
            let start = Instant::now();
            match llm.warm_up(&system) {
                Ok(()) => info!(elapsed = ?start.elapsed(), "language model warmed for the new prompt"),
                Err(error) => warn!(%error, "language model warm-up failed"),
            }
        });
        if let Err(error) = spawned {
            warn!(%error, "could not start the language model warm-up");
        }
    }

    /// "Undo": whichever changed last, the lights or the shopping list.
    fn undo(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let lights_at = self.last_lights.as_ref().filter(|(.., at)| at.elapsed() < MEMORY).map(|(.., at)| *at);
        let list_at = self.last_list.as_ref().filter(|(_, at)| at.elapsed() < MEMORY).map(|(_, at)| *at);
        if list_at.is_some() && (lights_at.is_none() || list_at > lights_at) {
            let (change, _) = self.last_list.take().expect("checked above");
            let Some(ha) = &self.home_assistant else { return "Home Assistant isn't connected yet.".into() };
            if let Err(error) = self.shopping.undo(ha, &change) {
                return unreachable(&error, no);
            }
            info!(?change, "shopping list change undone");
            return match (no, change) {
                (false, Change::Added(items)) => format!("Okay, took {} off again.", items.join(", ").to_lowercase()),
                (true, Change::Added(items)) => format!("Ok, fjernet {} igjen.", items.join(", ").to_lowercase()),
                (false, Change::Removed(items)) => {
                    format!("Okay, {} is back on the list.", items.join(", ").to_lowercase())
                }
                (true, Change::Removed(items)) => {
                    format!("Ok, {} er tilbake på lista.", items.join(", ").to_lowercase())
                }
            };
        }
        match self.recent_lights() {
            Some((target, on)) => self.switch(&target, !on, lang, true),
            None if no => "Det er ingenting å angre.".into(),
            None => "There's nothing to undo.".into(),
        }
    }

    fn shopping_list(&mut self, command: ListCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        self.pending_list_add = command == ListCommand::Add(Vec::new());
        match self.shopping.run(ha, &command, lang) {
            Ok((answer, change)) => {
                info!(?command, ?change, "shopping list");
                if let Some(change) = change {
                    self.last_list = Some((change, Instant::now()));
                }
                answer
            }
            Err(error) => unreachable(&error, no),
        }
    }

    /// "Which lights are on?": the rooms with lights on; with a room or light named, yes or no.
    fn lights_status(&mut self, request: &str, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let on = match ha.lights_on() {
            // Network gear reports its status LED as a light; nobody means that.
            Ok(on) => on.into_iter().filter(|l| !l.name.ends_with(" LED")).collect::<Vec<_>>(),
            Err(error) => return unreachable(&error, no),
        };
        let question: String = request
            .split_whitespace()
            .filter(|w| {
                !["anything", "everything", "something", "left", "still", "noe", "alt", "fortsatt"]
                    .contains(&voice_assistant::dialog::normalize(w).as_str())
            })
            .collect::<Vec<_>>()
            .join(" ");
        if ha::names_something(&question) && !mentions_this_room(request) {
            if let Ok(Some(target)) = ha.find(&question) {
                let lit = on.iter().any(|l| target.ids.contains(&l.id));
                let label = target.label.to_lowercase();
                return match (no, lit, target.area) {
                    (false, true, true) => format!("Yes, the {label} lights are on."),
                    (false, false, true) => format!("No, the {label} lights are off."),
                    (false, true, false) => format!("Yes, the {label} is on."),
                    (false, false, false) => format!("No, the {label} is off."),
                    (true, true, _) => format!("Ja, lyset i {label} er på."),
                    (true, false, _) => format!("Nei, lyset i {label} er av."),
                };
            }
        }
        // One name per room: the area, or the light's own name when it has none.
        let mut places: Vec<String> = Vec::new();
        for light in &on {
            let place = if light.area.is_empty() { light.name.to_lowercase() } else { light.area.to_lowercase() };
            if !places.contains(&place) {
                places.push(place);
            }
        }
        let and = if no { "og" } else { "and" };
        let list = match places.as_slice() {
            [] => String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} {and} {last}", rest.join(", ")),
        };
        match (no, places.is_empty()) {
            (false, true) => "All the lights are off.".into(),
            (true, true) => "Alt lyset er av.".into(),
            (false, false) => format!("Lights are on in the {list}."),
            (true, false) => format!("Lyset er på i {list}."),
        }
    }

    fn scene(&mut self, request: &str, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let here = self.room.clone();
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let scenes = match ha.scenes() {
            Ok(scenes) => scenes.to_vec(),
            Err(error) => return unreachable(&error, no),
        };
        let scene = match ha::resolve_scene(&scenes, request, here.as_deref()) {
            ha::SceneMatch::Found(scene) => scene.clone(),
            ha::SceneMatch::WhichRoom => {
                self.pending_scene = Some(request.to_owned());
                return if no { "Hvilket rom?" } else { "Which room?" }.into();
            }
            ha::SceneMatch::NotFound if no => return "Jeg fant ikke den scenen.".into(),
            ha::SceneMatch::NotFound => return "I couldn't find that scene.".into(),
        };
        if let Err(error) = ha.activate(&scene) {
            return unreachable(&error, no);
        }
        info!(scene = scene.id, request, "scene activated");
        let name = scene.name.to_lowercase();
        if no {
            format!("Ok, {name}.")
        } else {
            format!("Okay, {name}.")
        }
    }

    fn script(&mut self, request: &str, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let script = match ha.scripts() {
            Ok(scripts) => ha::resolve_script(scripts, request).cloned(),
            Err(error) => return unreachable(&error, no),
        };
        let Some(script) = script else {
            return if no { "Jeg fant ikke det skriptet." } else { "I couldn't find that script." }.into();
        };
        if let Err(error) = ha.activate(&script) {
            return unreachable(&error, no);
        }
        info!(script = script.id, request, "script started");
        let name = script.name.to_lowercase();
        if no {
            format!("Ok, kjører {name}.")
        } else {
            format!("Okay, running {name}.")
        }
    }

    /// A script or scene whose whole name is the request; `None` when there is none.
    fn by_exact_name(&mut self, request: &str, lang: Lang) -> Option<String> {
        let ha = self.home_assistant.as_mut()?;
        let script = ha.scripts().ok().and_then(|scripts| ha::exact(scripts, request).cloned());
        if script.is_some() {
            return Some(self.script(request, lang));
        }
        let ha = self.home_assistant.as_mut()?;
        let scene = ha.scenes().ok().and_then(|scenes| ha::exact(scenes, request).cloned())?;
        Some(self.scene(&scene.name, lang))
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
                self.keep_warm();
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
