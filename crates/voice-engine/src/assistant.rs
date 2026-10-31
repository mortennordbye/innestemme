//! The `assistant` processor. The model thread listens and plays the reply; a worker thread
//! handles requests (transcription for Whisper, intent, tool, speech).
//!
//! An energy detector on the model thread cuts the microphone into utterances; the worker
//! transcribes each one (Parakeet, English only, or Whisper, English and Norwegian) and acts on
//! those that start with the name or follow a wake word.
//!
//! Half duplex: while a request is being handled or its reply plays, and for a second after, the
//! microphone is ignored so the assistant does not answer itself through a laptop speaker.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::broadcast;
use tracing::{info, warn};
use voice_assistant::alerts::{self, Watch};
use voice_assistant::dialog::WakeWord;
use voice_assistant::geo::{self, Location};
use voice_assistant::ha::{self, HomeAssistant, Light, Playing, Target};
use voice_assistant::house::{self, HouseCommand, Routine, Switch};
use voice_assistant::intent::{self, Day, Intent, LightLevel, MusicCommand};
use voice_assistant::jokes::Jokes;
use voice_assistant::lang::Lang;
use voice_assistant::llm::{self, Decision, Llm, Turn};
use voice_assistant::lookup::Lookup;
use voice_assistant::music::Player;
use voice_assistant::news::News;
use voice_assistant::party::{Pace, PartyCommand};
use voice_assistant::persona::Persona;
use voice_assistant::plan;
use voice_assistant::power::{self, Power};
use voice_assistant::shopping::{Change, ListCommand, ShoppingList};
use voice_assistant::timer::{self, Notice, TimerCommand, Timers};
use voice_assistant::transit::Transit;
use voice_assistant::tts::{self, Tts};
use voice_assistant::tv::{self, Tv, TvCommand};
use voice_assistant::vad::{self, Vad};
use voice_assistant::voice_gender::{self, Gender, VoiceGender};
use voice_assistant::weather::Weather;
use voice_assistant::whisper::{Transcribe, Transcript};
use voice_proto::{FRAME_SAMPLES, SAMPLE_RATE};
use voice_rt::noalloc::permit_alloc;

use crate::activity::{self, Activity, Outcome};
use crate::engine::{FrameProcessor, ProcessError};
use crate::learned::Learned;
use crate::metrics::Metrics;
use crate::party::{self, Party};
use crate::render::{Priority, RenderAhead};
use crate::screen::Screen;
use crate::unhandled::{Reason, UnhandledLog};

/// Covers the STT's 0.5 s delay plus the room's echo tail.
const DEAF_AFTER_REPLY_FRAMES: u64 = 13;
/// The rolling KV cache is fine for long sessions, but position ids keep growing; start over
/// between requests once this many steps have passed (about 13 minutes).
/// "Them" and "that" refer to the last lights for this long.
const MEMORY: Duration = Duration::from_secs(120);
/// How often the worker checks that the language model's prompt is still the one it warmed.
const WARM_CHECK: Duration = Duration::from_secs(600);
/// Earlier exchanges the language model sees, for follow-ups ("and tomorrow?").
const HISTORY_TURNS: usize = 4;
const HISTORY_MAX_AGE: Duration = Duration::from_secs(300);
/// After a bare name or an answer, speech within this time needs no name.
const FOLLOW_UP: Duration = Duration::from_secs(8);
/// How long "Turn on privacy mode?" waits for its "yes".
const CONFIRM_WINDOW: Duration = Duration::from_secs(30);
/// Speech without the name that repeats what was said this recently is the answer coming back from
/// the speaker: the player starts late, so the end of an answer can reach the follow-up window.
const ECHO_WINDOW: Duration = Duration::from_secs(30);
/// Music paused for a request plays on after this long with nothing heard (a wake with no request,
/// speech that was not speech), and at the latest after `HUSH_CAP`, so talk in the room cannot
/// keep it paused.
const HUSH_QUIET: Duration = Duration::from_secs(12);
const HUSH_CAP: Duration = Duration::from_secs(40);
/// From queuing an answer to the speaker having played it: its length and this for the player to
/// start. Music started or stopped sooner cuts the announcement off.
const ANSWER_START: Duration = Duration::from_millis(1500);
/// A short utterance that starts this soon after a device's wake word holds the end of the wake
/// word ("Oh", "What?"), not the request. A longer one is the name and the request in one breath.
const WAKE_TAIL: Duration = Duration::from_millis(500);
const WAKE_TAIL_LONGEST: Duration = Duration::from_millis(1600);
/// Questions answered from the forecast and prices fetched ahead, rendered after each refresh.
const DATA_QUESTIONS: &[&str] = &[
    "What's the weather?",
    "What's the weather tomorrow?",
    "What's the electricity price now?",
    "When is electricity cheapest today?",
    "When is electricity cheapest tomorrow?",
];
/// Unrecognised speech after a wake longer than this many words gets no "didn't catch that".
const LONGEST_REQUEST_WORDS: usize = 12;
/// How often Home Assistant's safety sensors are checked.
const ALERT_POLL: Duration = Duration::from_secs(5);
/// How often the home's forecast and electricity prices are fetched while idle, so asking for them
/// needs no request and no lead-in.
const DATA_REFRESH: Duration = Duration::from_secs(240);
/// How long the TV screen shows asked-for data (weather, departures) before the face is back.
const SCREEN_DATA: Duration = Duration::from_secs(120);
/// Music shows longer: it is often left on while it plays.
const SCREEN_MUSIC: Duration = Duration::from_secs(300);
/// The Hue alert the lockdown's lights pulse with lasts about 15 seconds: started again before it ends.
const ALARM_REPEAT: Duration = Duration::from_secs(14);
/// A lockdown nobody stands down stops blinking after this and stays red.
const ALARM_LONGEST: Duration = Duration::from_secs(30 * 60);
/// A scene, a joke or coming home: a moment, not something to read.
const SCREEN_MOMENT: Duration = Duration::from_secs(25);
/// After an item is added the list stays open this long: each short reply is more items.
const LIST_OPEN: Duration = Duration::from_secs(60);
/// A reply longer than this while the list is open is the room talking, not items.
const LIST_REPLY_WORDS: usize = 6;
/// Replies that close the list.
const LIST_DONE: &[&str] = &[
    "that's all",
    "that's it",
    "that is all",
    "that is it",
    "that's everything",
    "done",
    "i'm done",
    "all done",
    "nothing else",
    "nothing more",
    "no more",
    "finished",
    "det var alt",
    "det er alt",
    "ferdig",
    "ikke noe mer",
    "ikke mer",
];
/// Where a sentence's speech starts (about -38 dBFS, above Kokoro's noise floor), and how much before
/// that is kept (10 ms).
const SPEECH_ONSET: u16 = 400;
/// The end of the audio before the wake word fired that holds the wake word: 1.5 s at 16 kHz. More
/// takes in what was said before it, the TV or someone else.
const VOICE_AUDIO: usize = 24_000;
const ONSET_LEAD: usize = SAMPLE_RATE as usize / 100;

pub struct AssistantConfig {
    /// Place used when a weather question names none; also the home's name in answers.
    pub home: Option<String>,
    /// The home's street address or coordinates: exact weather, and the stops near it.
    pub address: Option<String>,
    /// Stops for departures (names or NSR ids) instead of the ones nearest the address.
    pub transit_stops: Vec<String>,
    /// Contact (email or URL) for the User-Agent that MET Norway asks for.
    pub contact: Option<String>,
    /// RSS feeds for headlines, `Name=URL` or a URL: English, Norwegian.
    pub news_feeds: (Vec<String>, Vec<String>),
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
    /// English answers in a butler's words, addressing the user this way ("sir").
    pub honorific: Option<String>,
    /// Female voices are addressed this way ("ma'am") and `honorific` is for male ones; a voice the
    /// classifier is not sure of gets neither. Needs `voice_gender`.
    pub honorific_female: Option<String>,
    /// Tells male and female voices apart in the wake word.
    pub voice_gender: Option<Arc<VoiceGender>>,
    /// Speech is kept once rendered: fixed sentences are rendered ahead of time by this voice, on a
    /// thread of its own, and slow answers open with a lead-in.
    pub renderer: Option<Box<dyn Tts>>,
    /// The sentences spoken most, rendered before the rest (after a change of voice, the ones
    /// this home hears are ready first).
    pub often_spoken: Vec<String>,
    /// Directory for a wav of every utterance (debugging recognition).
    pub dump_utterances: Option<std::path::PathBuf>,
    /// Where requests that no skill handled are logged.
    pub unhandled: Option<Arc<UnhandledLog>>,
    /// Corrections learned from requests said again after a miss.
    pub learned: Option<Arc<Learned>>,
    /// The TV's media player entity; default: the media player that is a TV.
    pub tv: Option<String>,
    /// The Plex server for "play <title> on Plex".
    pub plex: Option<tv::Plex>,
    /// The `/tv` page as the TV reaches it, opened in its browser for "show ... on the TV".
    pub screen_url: Option<String>,
    /// Speak at once when a smoke, gas, water or breaking glass sensor goes off.
    pub alerts: bool,
    /// Percent the bedroom lights that are on dim to at bedtime.
    pub bedtime_light: u8,
    /// The room lit on coming home, whatever the time; else this room after dark.
    pub home_room: Option<String>,
    /// Music on coming home: "liked", or a playlist's name.
    pub home_music: Option<String>,
    /// What a lockdown plays: a song search ("Mombasa by Hans Zimmer").
    pub lockdown_music: String,
    /// The music's volume during a lockdown, in percent; music already louder stays as loud.
    pub lockdown_volume: u8,
    /// The Hue lights following the music, when a bridge is configured.
    pub party: Party,
    /// Shopping list items written under their Norwegian names, and Whisper for items said alone.
    pub norwegian_groceries: Option<voice_assistant::norwegian::Norwegian>,
    /// What was asked and answered lately, for the web page.
    pub activity: Arc<Activity>,
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
    /// What the TV screen shows now (`Screen::show`'s message), or the face again.
    Show(Option<serde_json::Value>),
}

/// Listening switched off from the web page (guests, a long conversation in the room), for a while
/// or until it is switched on again. Only the wake word and requests are ignored: timers and
/// reminders still ring.
#[derive(Default)]
pub struct Pause {
    /// Unix seconds when listening resumes: 0 while listening, `u64::MAX` until switched on.
    until: AtomicU64,
}

impl Pause {
    /// Stops listening for `minutes`, or until `resume` without them.
    pub fn pause(&self, minutes: Option<u64>) {
        let until = minutes.map_or(u64::MAX, |m| unix_now().saturating_add(m.saturating_mul(60)));
        self.until.store(until, Relaxed);
    }

    pub fn resume(&self) {
        self.until.store(0, Relaxed);
    }

    /// When listening resumes, in unix seconds (`u64::MAX`: when switched on), if it is paused.
    pub fn until(&self) -> Option<u64> {
        let until = self.until.load(Relaxed);
        (until > unix_now()).then_some(until)
    }

    pub fn paused(&self) -> bool {
        self.until().is_some()
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Talks to the assistant from outside the audio path (a satellite bridge).
#[derive(Clone)]
pub struct AssistantHandle {
    jobs: mpsc::Sender<Job>,
    events: broadcast::Sender<AssistantEvent>,
    hear_now: Arc<AtomicBool>,
    interrupted: Arc<AtomicBool>,
    pub pause: Arc<Pause>,
    pub screen: Arc<Screen>,
    pub party: Party,
    pub activity: Arc<Activity>,
    voice_gender: Option<Arc<VoiceGender>>,
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

/// What the assistant would say to a phrase typed on the web page.
pub enum Preview {
    /// The answer, styled, and its audio at 24 kHz.
    Spoken { text: String, audio: Vec<i16> },
    /// The request was understood but ends without an answer ("never mind").
    Silent,
    /// Answering would act on the house (lights, music, timers), or needs the language model.
    NotRun(&'static str),
}

/// What a wake word tells of the assistant's name being said.
enum WakeName {
    /// The device's own wake word: no audio.
    Unknown,
    /// The engine's, with its audio (24 kHz) to check.
    Check(Vec<i16>),
    /// A near miss already checked: the name was in it.
    Heard,
}

enum Job {
    /// A session started: tell the speaker they can talk.
    Ready,
    /// A satellite heard its wake word itself, at this time, with what is known of the name in it.
    Woken(Instant, WakeName),
    /// Whether the name is in this audio (24 kHz): a near miss of the wake word.
    CheckName(Vec<i16>, tokio::sync::oneshot::Sender<Option<bool>>),
    /// A satellite heard nothing after its wake word.
    NothingAsked,
    /// Audio still to transcribe, and when it ended.
    Utterance(Vec<i16>, Instant),
    /// The speaker paused and may be done. Transcribed ahead of the utterance.
    Draft(Vec<i16>),
    /// A satellite connected, by its names.
    Satellite(Vec<String>),
    /// How the wake word's voice sounded: male, female, or not sure.
    Voice(Option<Gender>),
    /// The web page's "hear the answer".
    Preview(String, tokio::sync::oneshot::Sender<Preview>),
    /// The hub's "what would it do": the lights a request changes, as JSON.
    Plan(String, tokio::sync::oneshot::Sender<serde_json::Value>),
    /// A safety sensor went off: speak this warning.
    Alert(String),
    /// A Home Assistant call the answer did not wait for failed: say so.
    Failed(String),
    /// A button on the web page: a fixed request, acted on without a spoken answer. The answer's
    /// text comes back.
    Act(String, tokio::sync::oneshot::Sender<String>),
}

pub struct AssistantProcessor {
    vad: Vad,
    jobs: mpsc::Sender<Job>,
    events: broadcast::Sender<AssistantEvent>,
    reply: rtrb::Consumer<i16>,
    /// Set from dispatch until the reply is queued.
    busy: Arc<AtomicBool>,
    /// A satellite started listening: stop ignoring the microphone at once.
    hear_now: Arc<AtomicBool>,
    interrupted: Arc<AtomicBool>,
    pause: Arc<Pause>,
    screen: Arc<Screen>,
    party: Party,
    activity: Arc<Activity>,
    voice_gender: Option<Arc<VoiceGender>>,
    was_busy: bool,
    frame: u64,
    deaf_until: u64,
}

impl AssistantProcessor {
    /// `transcriber` is Whisper or Parakeet, one call per utterance the energy detector cuts.
    pub fn new(transcriber: Box<dyn Transcribe>, tts: Box<dyn Tts>, config: AssistantConfig) -> anyhow::Result<Self> {
        let (jobs, rx) = mpsc::channel();
        let (reply_tx, reply) = rtrb::RingBuffer::new(SAMPLE_RATE as usize * 60);
        let busy = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(Pause::default());
        let screen = Arc::new(Screen::default());
        let party = config.party.clone();
        let activity = config.activity.clone();
        let voice_gender = config.voice_gender.clone().filter(|_| config.honorific_female.is_some());
        let speaker = config.speaker.clone();
        let tv = Tv::new(config.tv.clone()).with_plex(config.plex.clone());
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
        let news = News::new(&user_agent, &config.news_feeds.0, &config.news_feeds.1);
        let shopping = match config.norwegian_groceries {
            Some(_) => ShoppingList::default().with_norwegian_names(),
            None => ShoppingList::default(),
        };
        let mut worker = Worker {
            events: events.clone(),
            reply: reply_tx,
            busy: busy.clone(),
            interrupted: interrupted.clone(),
            pause: pause.clone(),
            screen: screen.clone(),
            tts,
            config,
            weather: Weather::new(&user_agent),
            news,
            lookup: Lookup::new(&user_agent),
            live: HashSet::new(),
            transit,
            power,
            home_location,
            jokes: Jokes::default(),
            home_assistant: None,
            player: Player::new(speaker),
            tv,
            transcriber,
            draft: None,
            follow_up_until: None,
            woken_at: None,
            wake_named: None,
            said: VecDeque::new(),
            answered_draft: None,
            hushed: None,
            speech_ended: None,
            history: VecDeque::new(),
            last_lights: None,
            pending_lights: None,
            shopping,
            last_list: None,
            list_open: None,
            request_audio: Vec::new(),
            turn: None,
            pending_scene: None,
            pending_confirm: None,
            timers: Timers::default(),
            persona: None,
            render: None,
            lead_turn: 0,
            chat_turn: 0,
            last_audio: None,
            room: None,
            warmed_prompt: String::new(),
            lockdown_volume: None,
            lockdown_tv: None,
            threat_turn: 0,
            music_under_answer: false,
            after_answer: None,
            after_answer_at: None,
            alarm: None,
            refreshed: None,
        };
        worker.room = worker.config.room.clone();
        worker.persona = worker.config.honorific.as_deref().map(|h| match &worker.config.honorific_female {
            Some(female) if worker.config.voice_gender.is_some() => Persona::new(h).with_others(&[female, ""]),
            _ => Persona::new(h),
        });
        if let Some(renderer) = worker.config.renderer.take() {
            let render = RenderAhead::spawn(renderer)?;
            let (soon, idle) = worker.fixed_sentences();
            info!(
                often = worker.config.often_spoken.len(),
                soon = soon.len(),
                idle = idle.len(),
                "rendering speech ahead"
            );
            render.add(Priority::Soon, std::mem::take(&mut worker.config.often_spoken));
            render.add(Priority::Soon, soon);
            render.add(Priority::Idle, idle);
            worker.render = Some(render);
        }
        if let Some((url, token)) = &worker.config.home_assistant {
            let mut ha = HomeAssistant::new(url, token);
            // Once for the calls of one request: a lockdown makes three.
            let (failed, last) = (jobs.clone(), std::sync::Mutex::new(None::<Instant>));
            ha.write_behind(move |error| {
                let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
                if last.is_none_or(|at| at.elapsed() > Duration::from_secs(10)) {
                    *last = Some(Instant::now());
                    let _ = failed.send(Job::Failed(unreachable_text(error, false).into()));
                }
            })?;
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
            if let Some(render) = worker.render.clone() {
                let home = worker.home_sentences();
                info!(sentences = home.len(), "rendering this home's answers ahead");
                render.add(Priority::Soon, home);
            }
        }
        if let Some((url, token)) = worker.config.home_assistant.clone().filter(|_| worker.config.alerts) {
            let jobs = jobs.clone();
            std::thread::Builder::new()
                .name("alerts".into())
                .spawn(move || watch_alerts(HomeAssistant::new(&url, &token), jobs))?;
            info!("watching home assistant for safety alerts");
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
        let start = Instant::now();
        worker.transcriber.transcribe(&[0i16; FRAME_SAMPLES * 12])?;
        info!(elapsed = ?start.elapsed(), "listener warmed up");
        std::thread::Builder::new().name("assistant".into()).spawn(move || worker.run(rx))?;
        let hear_now = Arc::new(AtomicBool::new(false));
        Ok(Self {
            vad: Vad::default(),
            jobs,
            events,
            reply,
            busy,
            hear_now,
            interrupted,
            pause,
            screen,
            party,
            activity,
            voice_gender,
            was_busy: false,
            frame: 0,
            deaf_until: 0,
        })
    }

    pub fn handle(&self) -> AssistantHandle {
        AssistantHandle {
            jobs: self.jobs.clone(),
            events: self.events.clone(),
            hear_now: self.hear_now.clone(),
            interrupted: self.interrupted.clone(),
            pause: self.pause.clone(),
            screen: self.screen.clone(),
            party: self.party.clone(),
            activity: self.activity.clone(),
            voice_gender: self.voice_gender.clone(),
        }
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
        // Interrupted by a satellite's wake word: what is still being synthesized is dropped, and the
        // request said over it must be heard.
        let interrupted = self.interrupted.load(Relaxed);
        if interrupted {
            self.deaf_until = 0;
            let queued = self.reply.slots();
            if let Ok(chunk) = self.reply.read_chunk(queued) {
                chunk.commit_all();
            }
        }
        let busy = self.busy.load(Relaxed) && !interrupted;
        if busy || self.was_busy {
            let queued = (self.reply.slots() / FRAME_SAMPLES) as u64;
            self.deaf_until = self.deaf_until.max(self.frame + queued + DEAF_AFTER_REPLY_FRAMES);
        }
        self.was_busy = busy;
        let deaf = self.frame < self.deaf_until;

        let start = Instant::now();
        permit_alloc(|| {
            if deaf {
                self.vad.reset();
                return;
            }
            if let Some(utterance) = self.vad.push(input) {
                info!(seconds = utterance.len() as f32 / SAMPLE_RATE as f32, "utterance");
                let _ = self.jobs.send(Job::Utterance(utterance, Instant::now()));
            }
            if let Some(draft) = self.vad.draft() {
                let _ = self.jobs.send(Job::Draft(draft));
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
        Ok(())
    }

    fn reset(&mut self) {
        permit_alloc(|| self.vad.reset());
        self.deaf_until = 0;
        let queued = self.reply.slots();
        if let Ok(chunk) = self.reply.read_chunk(queued) {
            chunk.commit_all();
        }
        let _ = permit_alloc(|| self.jobs.send(Job::Ready));
    }
}

/// What an answer has put out so far.
#[derive(Default)]
struct Spoken {
    samples: usize,
    first: Option<Instant>,
}

const LEADS_WEATHER: &[&str] = &["Checking the forecast", "Let me look at the weather", "Consulting the skies"];
const LEADS_TRANSIT: &[&str] =
    &["Checking the departures", "Let me look up the next departures", "Checking the timetable"];
const LEADS_POWER: &[&str] =
    &["Checking the electricity prices", "Let me look at the power prices", "Checking the price of power"];
const LEADS_LIGHTS: &[&str] = &["Let me check the lights", "Checking the lights", "Looking around the house"];
const LEADS_HOME: &[&str] = &["Let me see who's in", "Checking who's home", "Let me take a head count"];
const LEADS_THINK: &[&str] = &["Let me think", "Give me a moment", "Let me consider that"];
const LEADS_NEWS: &[&str] = &["Fetching the headlines", "Let me check the news", "Checking the papers"];
const LEADS_LOOKUP: &[&str] = &["Let me look that up", "One moment", "Let me find out"];
const LEADS_HOUSE: &[&str] = &["Let me check the house", "Checking the house", "Doing the rounds"];
const LEADS_TV: &[&str] = &["Let me find it", "Looking it up", "Finding it for you"];
const LEADS: &[&[&str]] = &[
    LEADS_WEATHER,
    LEADS_TRANSIT,
    LEADS_POWER,
    LEADS_LIGHTS,
    LEADS_HOME,
    LEADS_THINK,
    LEADS_NEWS,
    LEADS_LOOKUP,
    LEADS_HOUSE,
    LEADS_TV,
];

/// "Checking the forecast, sir." or "Checking the forecast."
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

fn lead_sentence(lead: &str, honorific: Option<&str>) -> String {
    match honorific {
        Some(h) if !h.is_empty() => format!("{lead}, {h}."),
        _ => format!("{lead}."),
    }
}

/// The lockdown's red lights blinking, on a thread of their own with their own Home Assistant
/// client, so answers are not held up.
struct Alarm {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Alarm {
    fn start(ha: HomeAssistant, red: Vec<String>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::Builder::new().name("alarm".into()).spawn(move || {
            // The lights go red on the write queue; a pulse before that would flash the old colours.
            ha.flush();
            let start = Instant::now();
            let targets = house::alarm_targets(&ha, &red).unwrap_or_else(|error| {
                warn!(error = format!("{error:#}"), "no Hue rooms for the lockdown; each light alone");
                red
            });
            info!(?targets, "lockdown blink");
            while !stopped.load(Relaxed) && start.elapsed() < ALARM_LONGEST {
                if let Err(error) = house::alarm_pulse(&ha, &targets) {
                    warn!(error = format!("{error:#}"), "lockdown blink failed");
                }
                // In short sleeps, so standing down is not kept waiting.
                let pulse = Instant::now();
                while !stopped.load(Relaxed) && pulse.elapsed() < ALARM_REPEAT {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            info!(after = ?start.elapsed(), "lockdown blink stopped");
        })?;
        Ok(Self { stop, thread })
    }

    /// Waits for a pulse in flight, so nothing is started after this returns.
    fn stop(self) {
        self.stop.store(true, Relaxed);
        let _ = self.thread.join();
    }
}

/// Players paused for a request.
struct Hushed {
    players: Vec<Playing>,
    at: Instant,
    last_heard: Instant,
}

struct Worker {
    events: broadcast::Sender<AssistantEvent>,
    reply: rtrb::Producer<i16>,
    busy: Arc<AtomicBool>,
    /// Set by a satellite when the wake word is said over an answer.
    interrupted: Arc<AtomicBool>,
    pause: Arc<Pause>,
    screen: Arc<Screen>,
    tts: Box<dyn Tts>,
    config: AssistantConfig,
    weather: Weather,
    transit: Transit,
    /// Electricity prices; `None` outside Norway or without a price area.
    power: Option<Power>,
    news: News,
    lookup: Lookup,
    /// Sentences of the answer being spoken that change daily without numbers (headlines): not
    /// kept by the speech cache.
    live: HashSet<String>,
    /// The home from the `address` setting.
    home_location: Option<Location>,
    jokes: Jokes,
    home_assistant: Option<HomeAssistant>,
    player: Player,
    tv: Tv,
    transcriber: Box<dyn Transcribe>,
    /// The latest draft and its transcripts.
    draft: Option<(Vec<i16>, Vec<Transcript>)>,
    /// Until then, speech counts without the name: after a bare name, and for a while after each
    /// answer, like a person who is still looking at you. Only recognised requests are acted on.
    follow_up_until: Option<Instant>,
    /// A satellite woke on its own wake word and nothing has been answered since.
    woken_at: Option<Instant>,
    /// Whether that wake word's audio held the assistant's name, when the engine detected it.
    wake_named: Option<bool>,
    /// Sentences spoken lately, to tell the answer coming back through the microphone from a request.
    said: VecDeque<(Instant, String)>,
    /// A command was acted on from its draft, of this many samples: the utterance it ends in is
    /// not answered again.
    answered_draft: Option<usize>,
    /// The music was paused for a request, and when something was last heard. It plays on after
    /// the answer, unless the request was to stop it.
    hushed: Option<Hushed>,
    /// When the speaker stopped talking, for how long the answer took to start.
    speech_ended: Option<Instant>,
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
    /// When an item was last added: the list takes more without "add" until `LIST_OPEN` later.
    list_open: Option<Instant>,
    /// The audio of the request being answered, for a second listener (Norwegian items).
    request_audio: Vec<i16>,
    /// The request being answered, recorded in `activity` once its answer is out.
    turn: Option<activity::Turn>,
    /// A scene request that exists in several rooms: the next answer is the room.
    pending_scene: Option<String>,
    /// A camera or alarm change waiting for "yes": a misheard request must not switch them.
    pending_confirm: Option<(HouseCommand, Instant)>,
    timers: Timers,
    persona: Option<Persona>,
    /// Renders speech ahead of time, when there is a speech cache.
    render: Option<RenderAhead>,
    lead_turn: usize,
    /// Small talk answers take turns.
    chat_turn: usize,
    /// The current utterance's recording, for the unhandled log.
    last_audio: Option<String>,
    /// Where the microphone is: the `room` setting, else the satellite's area.
    room: Option<String>,
    /// The system prompt the language model last saw. It holds the date and the room; a new one
    /// costs seconds of prompt processing on a CPU, so it is sent ahead of the next question.
    warmed_prompt: String,
    /// The music's volume before a lockdown raised it, for standing down.
    lockdown_volume: Option<f64>,
    /// The TV a lockdown put its screen on, and what the TV showed before, for standing down.
    lockdown_tv: Option<(String, Option<String>)>,
    /// Which threat the next lockdown ends on.
    threat_turn: usize,
    /// `after_answer` starts as the answer starts playing, not when it has played.
    music_under_answer: bool,
    /// Music to start or stop, and the volume to set, once the answer has played: the lockdown's
    /// music and its stand-down.
    after_answer: Option<(MusicCommand, Option<f64>)>,
    /// When the answer will have played, for `after_answer`.
    after_answer_at: Option<Instant>,
    /// The lockdown's lights blinking.
    alarm: Option<Alarm>,
    /// When the forecast and prices were last fetched ahead of a question.
    refreshed: Option<Instant>,
}

impl Worker {
    /// A wake, a request or its answer, or the few seconds after it that still count.
    fn talking(&self) -> bool {
        self.woken_at.is_some()
            || self.hushed.is_some()
            || self.busy.load(Relaxed)
            || self.follow_up_until.is_some_and(|until| Instant::now() < until)
    }

    fn run(mut self, jobs: mpsc::Receiver<Job>) {
        // Jobs taken off the channel early, to see whether a draft is still worth transcribing.
        let mut queued = VecDeque::new();
        loop {
            // Wake up for the next timer, and now and then to keep the language model warm.
            let next_timer = self.timers.next_deadline().map(|at| at.saturating_duration_since(Instant::now()));
            let wait = next_timer.map_or(WARM_CHECK, |t| t.min(WARM_CHECK));
            // During a conversation, to let rendering ahead go on soon after it.
            let wait = if self.talking() { wait.min(Duration::from_secs(1)) } else { wait };
            let wait = wait.min(DATA_REFRESH);
            let wait = match self.after_answer_at {
                Some(at) => wait.min(at.saturating_duration_since(Instant::now())),
                None => wait,
            };
            let job = match queued.pop_front().map_or_else(|| jobs.recv_timeout(wait), Ok) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => {
                    if self
                        .hushed
                        .as_ref()
                        .is_some_and(|h| h.last_heard.elapsed() > HUSH_QUIET || h.at.elapsed() > HUSH_CAP)
                    {
                        self.unhush();
                    }
                    self.ring_due_timers();
                    if self.after_answer_at.is_some_and(|at| Instant::now() >= at) {
                        self.run_after_answer();
                    }
                    let talking = self.talking();
                    if let Some(render) = &self.render {
                        render.hold(talking);
                    }
                    self.keep_warm();
                    if !talking && self.refreshed.is_none_or(|at| at.elapsed() >= DATA_REFRESH) {
                        self.refresh_data();
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            // A wake word or request while paused is dropped; a give-up still ends a listening run.
            let heard = matches!(job, Job::Woken(..) | Job::Utterance(..) | Job::Draft(_));
            if heard && self.pause.paused() {
                continue;
            }
            if let (true, Some(hushed)) = (heard, &mut self.hushed) {
                hushed.last_heard = Instant::now();
            }
            if let (true, Some(render)) = (heard, &self.render) {
                render.hold(true);
            }
            match job {
                Job::NothingAsked => {
                    self.woken_at = None;
                    self.unhush();
                }
                Job::Woken(at, wake) => {
                    info!("device wake word");
                    self.woken_at = Some(at);
                    // Until the wake word's voice is told (`Job::Voice`), which follows at once.
                    self.address(None);
                    self.follow_up_until = Some(Instant::now() + FOLLOW_UP);
                    self.emit(AssistantEvent::Listening);
                    self.hush();
                    // Checked while the request is still being said: done when a reply needs it,
                    // it held up "didn't catch that" by a quarter of a second on the cluster.
                    self.wake_named = match wake {
                        WakeName::Unknown => None,
                        WakeName::Check(audio) => self.name_in(&audio),
                        WakeName::Heard => Some(true),
                    };
                }
                Job::CheckName(audio, reply) => {
                    let _ = reply.send(self.name_in(&audio));
                }
                Job::Satellite(names) => self.satellite(&names),
                Job::Voice(gender) => self.address(gender),
                Job::Preview(text, reply) => {
                    let _ = reply.send(self.preview(&text));
                }
                Job::Plan(text, reply) => {
                    let _ = reply.send(self.plan(&text));
                }
                Job::Utterance(audio, ended) => self.utterance(&audio, ended),
                Job::Draft(audio) => {
                    queued.extend(jobs.try_iter());
                    // The speaker went on, or ended: a later job has newer audio.
                    if !queued.iter().any(|job| matches!(job, Job::Draft(_) | Job::Utterance(..))) {
                        self.transcribe_draft(audio);
                        self.act_on_draft();
                    }
                }
                Job::Alert(text) => {
                    info!(text, "safety alert");
                    self.announce(&text, tts::alarm_chime(), Lang::English, false);
                }
                Job::Failed(text) => self.announce(&text, Vec::new(), Lang::English, false),
                Job::Act(text, reply) => {
                    let start = Instant::now();
                    let answer = self.compose(&text, Lang::English).unwrap_or_default();
                    info!(request = text, answer, "done from the web page");
                    let turn = self.turn_for(&text, Some(answer.clone()), Outcome::Done, start);
                    self.config.activity.record(turn);
                    // What follows an answer (the lockdown's music) follows at once: there is none to wait out.
                    if self.after_answer.is_some() {
                        self.after_answer_at = Some(Instant::now());
                    }
                    let _ = reply.send(answer);
                }
                Job::Ready => {
                    info!("ready");
                    (self.follow_up_until, self.last_lights, self.pending_lights) = (None, None, None);
                    self.woken_at = None;
                    self.history.clear();
                    self.play(&tts::ready_chime());
                }
            }
        }
    }

    fn utterance(&mut self, audio: &[i16], ended: Instant) {
        // The utterance ended in the pause the draft was taken in: same speech, more silence.
        let closing = (vad::END_FRAMES - vad::DRAFT_FRAMES) as usize * FRAME_SAMPLES;
        if let Some(len) = self.answered_draft.take() {
            if audio.len() <= len + closing {
                info!("answered from the draft");
                return;
            }
        }
        // The utterance ends after a stretch of silence; the speaker stopped before it.
        let silence = Duration::from_secs_f32((vad::END_FRAMES as usize * FRAME_SAMPLES) as f32 / SAMPLE_RATE as f32);
        self.speech_ended = ended.checked_sub(silence);
        self.last_audio = None;
        if let Some(dir) = &self.config.dump_utterances {
            match save_wav(dir, "utterance", audio, SAMPLE_RATE) {
                Ok(path) => self.last_audio = path.file_name().map(|n| n.to_string_lossy().into_owned()),
                Err(error) => warn!(%error, "could not save utterance"),
            }
        }
        let transcriber = &mut self.transcriber;
        let start = Instant::now();
        let draft = self
            .draft
            .take()
            .filter(|(draft, _)| audio.len() == draft.len() + closing && audio.starts_with(draft))
            .map(|(_, candidates)| candidates);
        let from_draft = draft.is_some();
        let candidates = match draft.map_or_else(|| transcriber.transcribe(audio), Ok) {
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
        self.heard(&candidates, audio, ended, start, from_draft);
    }

    /// A draft that is a whole command with nothing that could follow ("pause the music", "what
    /// time is it") is acted on at once, without waiting out the rest of the closing silence.
    fn act_on_draft(&mut self) {
        let Some((_, candidates)) = &self.draft else { return };
        let Some(first) = candidates.first() else { return };
        let follow_up = self.follow_up_until.is_some_and(|until| Instant::now() < until);
        let request = match self.config.wake.strip(&first.text) {
            Some(rest) if !rest.is_empty() => rest,
            None if self.woken_at.is_some() => first.text.clone(),
            None if follow_up && !room_talk(&first.text) => first.text.clone(),
            _ => return,
        };
        let request = self.learned_meaning(request);
        if intent::split_commands(&request).is_some() || !complete_at_once(&intent::parse(&request)) {
            return;
        }
        let Some((audio, candidates)) = self.draft.take() else { return };
        let pause = Duration::from_secs_f32((vad::DRAFT_FRAMES as usize * FRAME_SAMPLES) as f32 / SAMPLE_RATE as f32);
        let now = Instant::now();
        self.speech_ended = now.checked_sub(pause);
        self.answered_draft = Some(audio.len());
        self.heard(&candidates, &audio, now, now, true);
    }

    /// What was said, as transcribed: a wake word, a request, or nothing for the assistant.
    fn heard(&mut self, candidates: &[Transcript], audio: &[i16], ended: Instant, start: Instant, from_draft: bool) {
        let follow_up = self.follow_up_until.is_some_and(|until| Instant::now() < until);
        // A translation usually drops the name, so a candidate that starts with the wake word is
        // the real transcript. Otherwise the most confident one.
        let heard = match candidates.iter().position(|c| self.config.wake.strip(&c.text).is_some()) {
            Some(i) if !follow_up => &candidates[i],
            _ => &candidates[0],
        };
        info!(text = heard.text, lang = ?heard.lang, took = ?start.elapsed(), from_draft, "heard");
        let request = match self.config.wake.strip(&heard.text) {
            Some(rest) if rest.is_empty() => {
                info!("wake word");
                self.follow_up_until = Some(Instant::now() + FOLLOW_UP);
                self.emit(AssistantEvent::Listening);
                self.hush();
                self.play(&tts::wake_chime());
                return;
            }
            Some(rest) => (rest, true),
            // While the device's wake run lasts, a request counts however long the pause before it.
            None if follow_up || self.woken_at.is_some() => (heard.text.clone(), false),
            None => {
                info!(text = heard.text, "not addressed");
                self.not_understood(&heard.text, audio, ended, heard.lang, start);
                return;
            }
        };
        let (request, addressed) = request;
        if !addressed && self.echo(&request) {
            info!(text = heard.text, "own answer heard");
            return;
        }
        // A bare "stop" right after an answer comes before any pause for a wake word: ask whether
        // music plays, as only then does it mean the music.
        let playing = self.hushed.is_some() || (bare_stop(&request) && self.music_playing());
        let request = match music_by_sound(&request, playing) {
            Some(meant) if addressed || self.woken_at.is_some() => {
                info!(heard = request, meant, "music request by its shape");
                meant
            }
            _ => request,
        };
        let request = self.learned_meaning(request);
        // Without the name, only act on something recognisable: in the follow-up window the
        // microphone also hears the room.
        // With a language model, it decides whether unrecognised words were meant for the assistant.
        let unknown = intent::parse(&request) == Intent::Unknown;
        if let (false, Some(learned)) = (unknown, &self.config.learned) {
            if let Some(pair) = learned.understood(&request) {
                info!(heard = pair.said, meant = pair.meant, "correction learned");
            }
        }
        // A question waiting for "yes" only takes a yes or a no: other words without the name are the room.
        let confirming = self.pending_confirm.is_some() && confirmation(&request).is_some();
        let pending =
            self.pending_lights.is_some() || confirming || self.timers.awaiting_duration() || self.list_is_open();
        // The follow-up window hears the room and the end of the answer too: "with a 40 year tariff
        // for murdering his barn it 40 stop" set the volume.
        let room = !addressed && self.woken_at.is_none() && room_talk(&request);
        if !pending && (room || (!addressed && unknown && self.config.llm.is_none())) {
            info!(text = heard.text, "not addressed");
            self.not_understood(&heard.text, audio, ended, heard.lang, start);
            return;
        }
        self.busy.store(true, Relaxed);
        self.request_audio = audio.to_vec();
        self.answer(&request, heard.lang, start);
        self.request_audio.clear();
    }

    /// The answer to a typed request, for requests that only read: they are answered for real
    /// (weather, departures, what is on), the rest are not run.
    fn preview(&mut self, text: &str) -> Preview {
        let request = self.config.wake.strip(text).unwrap_or_else(|| text.to_owned());
        let intent = intent::parse(&request);
        let reads = matches!(
            intent,
            Intent::Weather { .. }
                | Intent::Time
                | Intent::Joke
                | Intent::Transit(_)
                | Intent::Power(_)
                | Intent::LightsStatus { .. }
                | Intent::WhosHome(_)
                | Intent::Briefing
                | Intent::News
                | Intent::Lookup(_)
                | Intent::Calculate(_)
                | Intent::Distance(_)
                | Intent::Thanks
                | Intent::SmallTalk(_)
                | Intent::Cancel
                | Intent::ShoppingList(ListCommand::Read)
                | Intent::House(
                    HouseCommand::Status
                        | HouseCommand::Sun(_)
                        | HouseCommand::PhoneBattery(_)
                        | HouseCommand::GuestWifi(Switch::Status)
                        | HouseCommand::Camera(Switch::Status)
                        | HouseCommand::Alarm(Switch::Status | Switch::Off)
                        | HouseCommand::Climate(..)
                )
        );
        let answer = match intent {
            _ if reads => match self.compose(&request, Lang::English) {
                Some(answer) => answer,
                None => return Preview::Silent,
            },
            Intent::Unknown if self.config.llm.is_none() => "Sorry, I didn't catch that.".to_owned(),
            Intent::Unknown => {
                return Preview::NotRun("The language model would decide; it is not asked from the page.")
            }
            _ => return Preview::NotRun("Answering this switches or plays something, so the page does not run it."),
        };
        let answer = match &mut self.persona {
            Some(persona) => persona.style(&answer, jiff::Zoned::now().hour() as u8),
            None => answer,
        };
        let mut audio = Vec::new();
        for sentence in answer.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty()) {
            if let Err(error) = self.tts.speak(sentence, Lang::English, &mut |pcm| audio.extend_from_slice(pcm)) {
                warn!(%error, "speech synthesis failed");
            }
        }
        info!(request, answer, "preview for the web page");
        Preview::Spoken { text: answer, audio }
    }

    /// What a typed request would do to each light, worked out by the code that runs it, without
    /// running it: `{"request", "lights": {id: {kind, text}}, "also": [...]}`, or `{"reason"}` when
    /// it would switch nothing.
    fn plan(&mut self, text: &str) -> serde_json::Value {
        let request = self.config.wake.strip(text).unwrap_or_else(|| text.to_owned());
        let intent = intent::parse(&request);
        let reason = |reason: &str| serde_json::json!({ "request": request, "reason": reason });
        let lamps = match &self.home_assistant {
            None => return reason("Home Assistant isn't connected yet."),
            Some(ha) => match plan::lamps(ha) {
                Ok(lamps) => lamps,
                Err(error) => return reason(&format!("Home Assistant didn't answer: {error:#}")),
            },
        };
        match self.plan_of(&intent, &lamps) {
            Ok(mut plan) => {
                for (id, effect) in &mut plan.lights {
                    if effect.kind == "off" && ha::power_cycled(id) {
                        effect.text = "off, power back on".into();
                    }
                }
                info!(request, lights = plan.lights.len(), "light preview for the hub");
                serde_json::json!({ "request": request, "lights": plan.lights, "also": plan.also })
            }
            Err(why) => reason(&why),
        }
    }

    fn plan_of(&mut self, intent: &Intent, lamps: &[plan::Lamp]) -> Result<plan::Plan, String> {
        let mut plan = plan::Plan::default();
        let target = |this: &mut Self, request: &str| match this.light_target(request, Lang::English) {
            Ok(Some(target)) => Ok(target),
            Ok(None) => Err("It would ask which room.".to_owned()),
            Err(answer) => Err(answer),
        };
        let here = self.room.clone();
        let failed = |error: anyhow::Error| format!("Home Assistant didn't answer: {error:#}");
        match intent {
            Intent::Lights { on, target: request } => {
                let found = target(self, request)?;
                plan.set(lamps, &found.ids, |_| plan::Effect::switch(*on));
            }
            Intent::LightLevel { level, target: request } => {
                let found = target(self, request)?;
                plan.set(lamps, &found.ids, |l| plan::Effect::level(level, l.on));
            }
            Intent::Undo => {
                let Some((found, on)) = self.recent_lights() else {
                    return Err("There's nothing to undo.".into());
                };
                plan.set(lamps, &found.ids, |_| plan::Effect::switch(!on));
            }
            Intent::Scene(request) => {
                let ha = self.home_assistant.as_mut().ok_or("Home Assistant isn't connected yet.")?;
                let scenes = ha.scenes().map_err(failed)?.to_vec();
                let scene = match ha::resolve_scene(&scenes, request, here.as_deref()) {
                    ha::SceneMatch::Found(scene) => scene.clone(),
                    ha::SceneMatch::WhichRoom => return Err("It would ask which room.".into()),
                    ha::SceneMatch::NotFound => return Err("There is no such scene.".into()),
                };
                // Hue scenes do not list their lights in Home Assistant: the scene's room.
                let room = ha.find(&scene.area).map_err(failed)?.map(|t| t.ids).unwrap_or_default();
                plan.set(lamps, &room, |_| plan::Effect::new("scene", scene.name.clone()));
            }
            Intent::Party(command) => {
                let ha = self.home_assistant.as_ref().ok_or("Home Assistant isn't connected yet.")?;
                match command {
                    PartyCommand::On { .. } if !self.config.party.configured() => {
                        return Err("Party mode needs a Hue bridge.".into())
                    }
                    // The bridge streams to its colour lights; the rest keep what they show.
                    PartyCommand::On { .. } => {
                        let (colour, _) = house::lockdown_lights(ha).map_err(failed)?;
                        plan.set(lamps, &colour, |_| plan::Effect::new("party", "follows the music"));
                    }
                    PartyCommand::Off => {
                        let ids = house::snapshot_lights(ha, house::PARTY_SCENE).map_err(failed)?;
                        let ids = ids.ok_or("There's no party on.")?;
                        plan.set(lamps, &ids, |_| plan::Effect::new("restore", "put back"));
                    }
                }
            }
            Intent::Routine(routine) => {
                let config = &self.config;
                let ha = self.home_assistant.as_mut().ok_or("Home Assistant isn't connected yet.")?;
                match routine {
                    Routine::Leaving | Routine::Bedtime => {
                        let bedtime = *routine == Routine::Bedtime;
                        let on = ha.lights_on().map_err(failed)?;
                        let off = house::to_switch_off(on.clone(), |l| bedtime && in_bedroom(l));
                        let ids: Vec<&str> = off.iter().map(|l| l.id.as_str()).collect();
                        plan.set(lamps, &ids, |_| plan::Effect::switch(false));
                        if bedtime {
                            let bedroom: Vec<Light> = on.into_iter().filter(in_bedroom).collect();
                            let text = format!("{} %, warm", config.bedtime_light);
                            plan.set(lamps, &bedtime_dimmed(&bedroom), |_| plan::Effect::new("level", text.clone()));
                        }
                        let snapshot = house::snapshot(ha).map_err(failed)?;
                        if !snapshot.playing.is_empty() {
                            plan.also.push("The music pauses.".into());
                        }
                        if !tv::on_tvs(ha).map_err(failed)?.is_empty() {
                            plan.also.push("The TV turns off.".into());
                        }
                    }
                    Routine::Home => {
                        let found = home_target(ha, config, here.as_deref()).map_err(failed)?;
                        let found = found.ok_or("Coming home would switch no lights now.")?;
                        let lit = plan_lit(lamps, &found.ids);
                        plan.set(lamps, &found.ids, |l| match lit {
                            true => plan::Effect::new("same", if l.on { "already on" } else { "left off" }),
                            false => plan::Effect::new("on", "on"),
                        });
                        if let Some(music) = &config.home_music {
                            plan.also.push(format!("Music starts: {music}."));
                        }
                    }
                    Routine::Lockdown => {
                        let (red, off) = house::lockdown_lights(ha).map_err(failed)?;
                        plan.set(lamps, &red, |_| plan::Effect::new("red", "red, pulsing"));
                        plan.set(lamps, &off, |_| plan::Effect::switch(false));
                        plan.also.push(format!(
                            "Plays {} at {} % or louder.",
                            config.lockdown_music, config.lockdown_volume
                        ));
                    }
                    Routine::StandDown => {
                        let ids = house::snapshot_lights(ha, house::LOCKDOWN_SCENE).map_err(failed)?;
                        let ids = ids.ok_or("There is no lockdown to end.")?;
                        plan.set(lamps, &ids, |_| plan::Effect::new("restore", "put back"));
                    }
                }
            }
            Intent::Unknown => return Err("Not understood without the language model.".into()),
            _ => return Err("This request doesn't switch any lights.".into()),
        }
        Ok(plan)
    }

    /// A device woke on its wake word and the request was not understood: say so, once, rather
    /// than end the run in silence, which sounds as if the device never woke.
    /// Whether a wake word's audio holds the assistant's name.
    fn name_in(&mut self, audio: &[i16]) -> Option<bool> {
        let transcriber = &mut self.transcriber;
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

    fn not_understood(&mut self, text: &str, audio: &[i16], ended: Instant, lang: Lang, start: Instant) {
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

    fn transcribe_draft(&mut self, audio: Vec<i16>) {
        let transcriber = &mut self.transcriber;
        self.draft = match transcriber.transcribe(&audio) {
            Ok(candidates) => Some((audio, candidates)),
            Err(error) => {
                warn!(%error, "draft transcription failed");
                None
            }
        };
    }

    fn answer(&mut self, text: &str, lang: Lang, start: Instant) {
        self.interrupted.store(false, Relaxed);
        self.emit(AssistantEvent::Heard { text: text.to_owned() });
        // A slow answer (weather, departures) opens with a lead-in rendered ahead of time, which
        // plays while the data is fetched and its sentence synthesized.
        let lead = self.lead_in(text, lang);
        // Its data kept, a slow skill's answer goes out at once, worded as after a lead-in: the forms
        // rendered ahead, with no honorific worked into the data sentence.
        let plain = lead.is_none() && self.data_kept(text, lang);
        let mut spoken = Spoken::default();
        if let Some(lead) = &lead {
            self.emit(AssistantEvent::Answer { text: lead.clone() });
            self.speak(lead, lang, &mut spoken);
        }
        let Some(answer) = self.compose(text, lang) else {
            info!(request = text, "conversation ended");
            self.follow_up_until = None;
            self.pending_lights = None;
            self.timers.clear_pending();
            if lead.is_some() {
                self.finish(spoken, start);
            } else {
                self.emit(AssistantEvent::Done);
                self.unhush();
                self.woken_at = None;
                self.busy.store(false, Relaxed);
            }
            return;
        };
        let failed = ["Sorry", "Beklager"].iter().any(|s| answer.starts_with(s));
        if !failed && intent::split_commands(text).is_none() && heard_by_itself(&intent::parse(text)) {
            info!(request = text, answer, after = ?start.elapsed(), "done without words");
            let after_ms = self.speech_ended.take().map(|ended| ended.elapsed().as_millis() as u64);
            let turn = self.turn_for(text, Some(answer.clone()), Outcome::Done, start);
            self.config.activity.record(activity::Turn { after_ms, ..turn });
            self.history.push_back((Instant::now(), Turn { user: text.to_owned(), assistant: answer }));
            self.follow_up_until = Some(Instant::now() + FOLLOW_UP);
            self.emit(AssistantEvent::Done);
            self.unhush();
            self.woken_at = None;
            self.busy.store(false, Relaxed);
            return;
        }
        info!(request = text, answer, ?lang, after = ?start.elapsed(), "answering");
        self.turn = Some(self.turn_for(text, Some(answer.clone()), Outcome::Answered, start));
        // The rules' own fallback, or a skill that could not do it ("Sorry, I couldn't find ...").
        let fallback = ["Sorry, I can't help with that yet.", "Beklager, det kan jeg ikke hjelpe med ennå."];
        if fallback.iter().any(|f| answer.starts_with(f)) {
            self.unhandled(Reason::Unknown, text, Some(&answer));
        } else if ["Sorry", "Beklager"].iter().any(|s| answer.starts_with(s) || answer.contains(&format!(". {s}"))) {
            self.unhandled(Reason::Apologised, text, Some(&answer));
        }
        self.history.push_back((Instant::now(), Turn { user: text.to_owned(), assistant: answer.clone() }));
        while self.history.len() > HISTORY_TURNS {
            self.history.pop_front();
        }
        let answer = match &mut self.persona {
            Some(persona) if lang == Lang::English && (lead.is_some() || plain) => persona.after_lead(&answer),
            Some(persona) if lang == Lang::English => persona.style(&answer, jiff::Zoned::now().hour() as u8),
            _ => answer,
        };
        if lead.is_none() {
            self.emit(AssistantEvent::Answer { text: answer.clone() });
        }
        self.speak(&answer, lang, &mut spoken);
        self.finish(spoken, start);
    }

    /// Whether words heard without the name are an answer spoken lately.
    fn echo(&self, heard: &str) -> bool {
        let recent: Vec<&str> =
            self.said.iter().filter(|(at, _)| at.elapsed() < ECHO_WINDOW).map(|(_, text)| text.as_str()).collect();
        is_echo(&recent, heard)
    }

    /// Speaks an answer and opens the follow-up window.
    fn say(&mut self, answer: String, lang: Lang, start: Instant) {
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
    fn speak(&mut self, text: &str, lang: Lang, spoken: &mut Spoken) {
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

    fn turn_for(&self, heard: &str, answer: Option<String>, outcome: Outcome, start: Instant) -> activity::Turn {
        activity::Turn {
            at: activity::now(),
            heard: heard.to_owned(),
            answer,
            skill: voice_assistant::catalogue::skill_of(&intent::parse(heard)).map(|s| s.name),
            outcome,
            after_ms: None,
            decide_ms: start.elapsed().as_millis() as u64,
        }
    }

    fn finish(&mut self, spoken: Spoken, start: Instant) {
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

    /// The lead-in for a request whose answer is slow to get or to say, when speech is rendered
    /// ahead of time. Not when a pending question takes the words ("Which room?" "The kitchen.").
    fn lead_in(&mut self, text: &str, lang: Lang) -> Option<String> {
        let pending = self.pending_lights.is_some()
            || self.list_is_open()
            || self.pending_scene.is_some()
            || self.pending_confirm.is_some()
            || self.timers.awaiting_duration();
        if lang != Lang::English || self.render.is_none() || pending {
            return None;
        }
        // Several requests: the first one's lead-in.
        let first = intent::split_commands(text).and_then(|parts| parts.into_iter().next());
        let leads = match intent::parse(first.as_deref().unwrap_or(text)) {
            // A lead-in plays before the answer, so with the data kept it only delays it.
            _ if self.data_kept(text, lang) => return None,
            Intent::Weather { .. } => LEADS_WEATHER,
            Intent::Transit(_) => LEADS_TRANSIT,
            Intent::Power(_) => LEADS_POWER,
            Intent::News => LEADS_NEWS,
            Intent::Lookup(_) => LEADS_LOOKUP,
            Intent::LightsStatus { .. } => LEADS_LIGHTS,
            Intent::WhosHome(_) => LEADS_HOME,
            Intent::House(HouseCommand::Status) => LEADS_HOUSE,
            Intent::Tv(TvCommand::Nrk(_) | TvCommand::Plex(_)) => LEADS_TV,
            Intent::Unknown if self.config.llm.is_some() => LEADS_THINK,
            _ => return None,
        };
        self.lead_turn = self.lead_turn.wrapping_add(1);
        Some(lead_sentence(leads[self.lead_turn % leads.len()], self.persona.as_ref().map(Persona::honorific)))
    }

    /// Whether a weather or electricity price request is answered from data fetched ahead.
    fn data_kept(&self, text: &str, lang: Lang) -> bool {
        let first = intent::split_commands(text).and_then(|parts| parts.into_iter().next());
        lang == Lang::English
            && match intent::parse(first.as_deref().unwrap_or(text)) {
                Intent::Weather { place: None, .. } => {
                    self.home_location.as_ref().is_some_and(|home| self.weather.is_fresh(home))
                }
                Intent::Power(query) => self.power.as_ref().is_some_and(|p| p.is_ready(query)),
                _ => false,
            }
    }

    /// Every fixed English sentence the assistant says, as spoken (a sentence at a time): what is
    /// said most, then the long tail.
    fn fixed_sentences(&self) -> (Vec<String>, Vec<String>) {
        let honorifics: Vec<Option<&str>> = match &self.persona {
            // Plain as well: a voice the persona is unsure of hears the lead-in without one.
            Some(persona) => persona.honorifics().iter().map(|h| Some(h.as_str())).chain([None]).collect(),
            None => vec![None],
        };
        let mut texts: Vec<String> = LEADS
            .iter()
            .flat_map(|leads| leads.iter())
            .flat_map(|l| honorifics.iter().map(|h| lead_sentence(l, *h)))
            .collect();
        texts.extend(self.persona.as_ref().map(Persona::phrases).unwrap_or_default());
        let questions =
            [HouseCommand::Camera(Switch::Off), HouseCommand::Camera(Switch::On), HouseCommand::Alarm(Switch::On)]
                .map(|c| confirm_question(&c, Lang::English).unwrap_or_default());
        let answers = voice_assistant::music::ENGLISH_ANSWERS
            .iter()
            .chain(&questions)
            .chain(&["Okay, I'll leave it.", "Anything else?", "Okay, that's everything."])
            .chain(voice_assistant::tv::ENGLISH_ANSWERS)
            .chain(&[
                "Okay, party mode is on.",
                "Okay, fast party mode is on.",
                "Okay, relaxed party mode is on.",
                "Okay, the party's over.",
            ])
            .chain(house::LOCKDOWN_ANSWERS)
            .chain(house::LOCKDOWN_THREATS)
            .chain(house::ROUTINE_ANSWERS)
            .chain(Jokes::english())
            .chain(&["Sorry, I didn't catch that."])
            .map(|a| a.to_string())
            .chain(voice_assistant::smalltalk::all_english(self.config.wake.name()));
        match &self.persona {
            Some(persona) => texts.extend(answers.flat_map(|a| persona.forms(&a))),
            None => texts.extend(answers),
        }
        let mut sentences: Vec<String> = texts
            .iter()
            .flat_map(|t| {
                t.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
            })
            .filter(|s| voice_assistant::speech_cache::CachedTts::cacheable(s))
            .collect();
        sentences.sort();
        sentences.dedup();
        // Then the answers with numbers that come up most, which the cache keeps too.
        let mut numbered: Vec<String> = (0..=100).map(|n| format!("Volume {n} percent.")).collect();
        for minutes in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 15, 20, 25, 30, 40, 45, 60, 90, 120] {
            numbered.push(format!("Timer set for {}.", timer::say_duration(minutes * 60, Lang::English)));
            if minutes < 60 {
                numbered.push(format!("The {minutes} minute timer is cancelled."));
            }
        }
        for seconds in [10, 15, 20, 30, 45, 90] {
            numbered.push(format!("Timer set for {}.", timer::say_duration(seconds, Lang::English)));
        }
        // Every time of day, from now on: the next hours are ready soon after a start, the rest in
        // idle time over a day, and then kept on disk.
        let now = jiff::Zoned::now();
        let minute = u32::from(now.hour() as u8) * 60 + u32::from(now.minute() as u8);
        let mut times: Vec<String> = (0..24 * 60)
            .map(|i| {
                let m = (minute + i) % (24 * 60);
                format!("It's {:02}:{:02}.", m / 60, m % 60)
            })
            .collect();
        let mut later = times.split_off(120);
        numbered.extend(times);
        let styled = |answers: Vec<String>| -> Vec<String> {
            answers
                .into_iter()
                .flat_map(|a| match &self.persona {
                    Some(persona) => persona.forms(&a),
                    None => vec![a],
                })
                .collect()
        };
        sentences.extend(styled(numbered));
        // Answers after a lead-in are spoken as they are, without the persona's opener: the weather,
        // departures and electricity prices, a sentence at a time. Today's own are rendered first,
        // after each refresh (`data_sentences`).
        let mut tail = Vec::new();
        let home = self.home_location.as_ref().map(|l| l.label.clone()).or_else(|| self.config.home.clone());
        if let Some(home) = home {
            tail.extend(voice_assistant::weather::english_sentences(&home));
        }
        tail.extend(voice_assistant::transit::english_sentences());
        tail.extend(voice_assistant::power::english_sentences());
        later.extend(timer::english_left_sentences());
        tail.extend(styled(later));
        let cacheable = voice_assistant::speech_cache::CachedTts::cacheable;
        sentences.retain(|s| cacheable(s));
        tail.retain(|s| cacheable(s));
        (sentences, tail)
    }

    /// Answers about this home's own lights and rooms, which Home Assistant names: the first
    /// "turn off the kitchen lights" would otherwise wait for its sentence to be synthesized.
    fn home_sentences(&mut self) -> Vec<String> {
        let Some(ha) = &mut self.home_assistant else { return Vec::new() };
        let states = ["on", "off", "back on", "back off"];
        let mut answers: Vec<String> = [
            "I couldn't find that light.",
            "I couldn't find that scene.",
            "Sorry, I couldn't reach Home Assistant.",
            "Sorry, Home Assistant couldn't do all of that.",
            "Sorry, I couldn't find that place.",
            "Okay, I'm ringing your phone.",
        ]
        .map(String::from)
        .into();
        if let Ok(lights) = ha.lights() {
            let mut areas: Vec<String> =
                lights.iter().map(|l| l.area.to_lowercase()).filter(|a| !a.is_empty()).collect();
            // "Turn off all the lights": every lamp, under the house's name.
            areas.push("house".to_owned());
            areas.sort();
            areas.dedup();
            for area in &areas {
                answers.extend(states.map(|state| format!("Okay, the {area} lights are {state}.")));
            }
            for light in lights.iter().filter(|l| !l.group) {
                let name = light.name.to_lowercase();
                answers.extend(states.map(|state| format!("Okay, the {name} is {state}.")));
            }
        }
        // Indoor temperatures, the ones a home has most of the year.
        if let Ok(rooms) = house::climate_readings(ha) {
            for room in rooms.iter().filter(|r| r.temperature.is_some()) {
                let area = room.area.to_lowercase();
                answers.extend((15..=30).map(|t| format!("It's {t} degrees in the {area}.")));
            }
        }
        self.spoken_sentences(&answers)
    }

    /// The sentences of these answers in every way the persona says them, as kept in the cache.
    fn spoken_sentences(&self, answers: &[String]) -> Vec<String> {
        let mut sentences: Vec<String> = answers
            .iter()
            .flat_map(|a| match &self.persona {
                Some(persona) => persona.forms(a),
                None => vec![a.clone()],
            })
            .flat_map(|t| {
                t.split_inclusive(['.', '?', '!'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|s| voice_assistant::speech_cache::CachedTts::cacheable(s))
            .collect();
        sentences.sort();
        sentences.dedup();
        sentences
    }

    /// Today's answers from the forecast and prices fetched ahead, to render before anyone asks.
    fn data_sentences(&mut self) -> Vec<String> {
        let kept: Vec<&str> = DATA_QUESTIONS.iter().copied().filter(|q| self.data_kept(q, Lang::English)).collect();
        let answers: Vec<String> = kept.into_iter().filter_map(|q| self.compose(q, Lang::English)).collect();
        self.spoken_sentences(&answers)
    }

    /// The home's forecast and the electricity prices, fetched before anyone asks.
    fn refresh_data(&mut self) {
        self.refreshed = Some(Instant::now());
        if let Some(home) = self.home_location.clone() {
            if let Err(error) = self.weather.refresh(&home) {
                warn!(error = format!("{error:#}"), "could not refresh the forecast");
            }
        }
        if let Some(power) = &mut self.power {
            if let Err(error) = power.refresh() {
                warn!(error = format!("{error:#}"), "could not refresh the electricity prices");
            }
        }
        if let Some(render) = self.render.clone() {
            render.add(Priority::Soon, self.data_sentences());
        }
    }

    /// The spoken answer, or `None` to end the conversation quietly.
    fn compose(&mut self, text: &str, lang: Lang) -> Option<String> {
        let intent = match intent::parse(text) {
            // A language model knows facts the opening of an article leaves out; it can still look
            // things up with its tool.
            Intent::Lookup(_) if self.config.llm.is_some() => Intent::Unknown,
            intent => intent,
        };
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
        // "Milk and eggs", "og brunost", "add bacon too": more items while the list is open.
        if self.list_open.take().is_some_and(|at| at.elapsed() < LIST_OPEN) {
            let said: Vec<String> =
                text.split_whitespace().map(voice_assistant::dialog::normalize).filter(|w| !w.is_empty()).collect();
            let said = said.join(" ");
            if LIST_DONE.contains(&said.as_str()) {
                return Some(
                    if lang == Lang::Norwegian { "Ok, det var alt." } else { "Okay, that's everything." }.into(),
                );
            }
            if intent == Intent::Unknown && text.split_whitespace().count() <= LIST_REPLY_WORDS {
                let items = self.items_heard(text);
                if !items.is_empty() {
                    return Some(self.shopping_list(ListCommand::Add(items), lang, true));
                }
            }
        }
        if let Some((command, at)) = self.pending_confirm.take() {
            match confirmation(text) {
                Some(true) if at.elapsed() < CONFIRM_WINDOW => return Some(self.house(&command, lang)),
                Some(false) => {
                    return Some(
                        if lang == Lang::Norwegian { "Ok, jeg lar det være." } else { "Okay, I'll leave it." }.into(),
                    )
                }
                // Anything else is a new request, and the question is dropped.
                _ => {}
            }
        }
        if let Some(scene) = self.pending_scene.take() {
            if intent == Intent::Unknown {
                return Some(self.scene(&format!("{scene} in {text}"), lang));
            }
        }
        if let Some(parts) = intent::split_commands(text) {
            return self.several(&parts, lang);
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
                        self.unhandled(Reason::LanguageModel, text, Some(&answer));
                        return Some(answer);
                    }
                    Ok(Decision::Ignore) => {
                        info!(request = text, took = ?start.elapsed(), "language model: not meant for me");
                        self.unhandled(Reason::LanguageModelIgnored, text, None);
                        return None;
                    }
                    Err(error) => warn!(%error, "language model failed; falling back to the rules"),
                }
            }
        }
        self.act(intent, lang)
    }

    fn unhandled(&self, reason: Reason, heard: &str, answer: Option<&str>) {
        if let Some(log) = &self.config.unhandled {
            log.record(reason, heard, answer, self.last_audio.as_deref());
        }
        if let (Reason::NotUnderstood | Reason::Unknown | Reason::FollowUpIgnored, Some(learned)) =
            (reason, &self.config.learned)
        {
            learned.failed(heard);
        }
    }

    /// The request learned for words that are not understood as they are; else the words.
    fn learned_meaning(&self, request: String) -> String {
        let learned = self.config.learned.as_ref().filter(|_| intent::parse(&request) == Intent::Unknown);
        match learned.and_then(|learned| learned.meant(&request)) {
            Some(meant) => {
                info!(heard = request, meant, "learned correction");
                meant
            }
            None => request,
        }
    }

    /// Several requests in one sentence, run in order; their answers one after another. A question
    /// ("Which room?") stops there, since the reply to it is the next thing said.
    fn several(&mut self, parts: &[String], lang: Lang) -> Option<String> {
        info!(?parts, "several requests");
        let mut answers = Vec::new();
        for part in parts {
            if let Some(answer) = self.act(intent::parse(part), lang) {
                let question = answer.trim_end().ends_with('?');
                // One "Okay," is enough: "Okay, the lights are on. Playing ..."
                let answer = match answer.strip_prefix("Okay, ") {
                    Some(rest) if !answers.is_empty() => capitalize(rest),
                    _ => answer,
                };
                answers.push(answer);
                if question {
                    break;
                }
            }
        }
        (!answers.is_empty()).then(|| answers.join(" "))
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
            Intent::Joke => {
                let joke = self.jokes.tell(lang);
                if self.screen.watched() {
                    self.show("joke", serde_json::json!({ "text": joke }), Some(SCREEN_MOMENT));
                }
                joke
            }
            Intent::Time => time_of_day(lang),
            Intent::Party(command) => self.party(command, lang),
            Intent::Music(command) => {
                // New music replaces what was paused for the request, and "play" plays it itself.
                if matches!(
                    command,
                    MusicCommand::Pause
                        | MusicCommand::Resume
                        | MusicCommand::Stop
                        | MusicCommand::Liked
                        | MusicCommand::Playlist(_)
                        | MusicCommand::Song(_)
                        | MusicCommand::Artist(_)
                        | MusicCommand::Album(_)
                ) {
                    self.hushed = None;
                }
                if command == MusicCommand::Stop {
                    self.party_off();
                }
                self.player.run(self.home_assistant.as_ref(), &command, lang)
            }
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
                        if no {
                            "Beklager, jeg får ikke hentet strømprisene."
                        } else {
                            "Sorry, I couldn't get the electricity prices."
                        }
                        .into()
                    }
                },
            },
            Intent::WhosHome(name) => self.whos_home(name.as_deref(), lang),
            Intent::Briefing => self.briefing(lang),
            Intent::Lookup(topic) => match self.lookup.answer(&topic, lang) {
                Ok(Some(answer)) => {
                    // An article's sentences change rarely but are endless: not for the speech cache.
                    let sentences = answer.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty());
                    self.live.extend(sentences.map(str::to_owned));
                    answer
                }
                Ok(None) if no => format!("Beklager, jeg fant ingenting om {topic}."),
                Ok(None) => format!("Sorry, I couldn't find anything about {topic}."),
                Err(error) => {
                    warn!(error = format!("{error:#}"), "lookup failed");
                    if no {
                        "Beklager, jeg får ikke slått det opp nå."
                    } else {
                        "Sorry, I couldn't look that up right now."
                    }
                    .into()
                }
            },
            Intent::News => match self.news.answer(lang) {
                Ok(headlines) => {
                    self.live.extend(headlines.live);
                    headlines.text
                }
                Err(error) => {
                    warn!(error = format!("{error:#}"), "news failed");
                    if no {
                        "Beklager, jeg får ikke hentet nyhetene nå."
                    } else {
                        "Sorry, I couldn't get the news right now."
                    }
                    .into()
                }
            },
            Intent::ShoppingList(command) => self.shopping_list(command, lang, false),
            Intent::Scene(request) => self.scene(&request, lang),
            Intent::Script(request) => self.script(&request, lang),
            Intent::Undo => self.undo(lang),
            Intent::Thanks if no => "Bare hyggelig.".into(),
            Intent::Thanks => "You're welcome.".into(),
            Intent::SmallTalk(chat) => {
                let answers = voice_assistant::smalltalk::answers(chat, lang, self.config.wake.name());
                self.chat_turn = self.chat_turn.wrapping_add(1);
                answers[self.chat_turn % answers.len()].clone()
            }
            Intent::Cancel => return None,
            // Not a list of skills: it keeps growing, and the web page shows it.
            Intent::Unknown if no => "Beklager, det kan jeg ikke hjelpe med ennå.".into(),
            Intent::Unknown => "Sorry, I can't help with that yet.".into(),
            Intent::Calculate(said) => match voice_assistant::calc::parse(&said) {
                Some((said, value)) => voice_assistant::calc::answer(&said, value, lang),
                None => return None,
            },
            Intent::Distance(query) => self.distance(&query, lang),
            Intent::Tv(TvCommand::Show(screen)) => self.show_screen(screen, lang),
            Intent::Tv(TvCommand::Hide) => {
                self.hide();
                if no { "Ok." } else { "Okay." }.into()
            }
            Intent::Tv(command) => self.tv.run(self.home_assistant.as_ref(), &command, lang),
            Intent::House(command) => match confirm_question(&command, lang) {
                Some(question) => {
                    self.pending_confirm = Some((command, Instant::now()));
                    question.into()
                }
                None => self.house(&command, lang),
            },
            Intent::Routine(routine) => {
                // Leaving and bedtime pause the music themselves; coming home may start new music.
                if routine != Routine::Home || self.config.home_music.is_some() {
                    self.hushed = None;
                }
                match routine {
                    Routine::Lockdown => self.lockdown(lang),
                    Routine::StandDown => self.stand_down(lang),
                    _ => self.routine(routine, lang),
                }
            }
        })
    }

    /// "Party mode": the Hue lights follow the music, after a snapshot to put them back from.
    fn party(&mut self, command: PartyCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let PartyCommand::On(pace) = command else {
            return match self.party_off() {
                true if no => "Ok, festen er over.",
                true => "Okay, the party's over.",
                false if no => "Det er ingen fest på gang.",
                false => "There's no party on.",
            }
            .into();
        };
        if !self.config.party.configured() {
            return if no {
                "Festmodus trenger en Hue-bro. Sett hue-bridge i innstillingene."
            } else {
                "Party mode needs a Hue bridge. Set hue-bridge in the settings."
            }
            .into();
        }
        if let Some(ha) = &self.home_assistant {
            if let Err(error) = house::party_snapshot(ha) {
                warn!(error = format!("{error:#}"), "party mode: no snapshot of the lights");
            }
        }
        let mode = match pace {
            Pace::Relaxed => party::Mode::Relaxed,
            Pace::Normal => party::Mode::Normal,
            Pace::Fast => party::Mode::Fast,
        };
        if let Err(error) = self.config.party.start(mode) {
            warn!(error = format!("{error:#}"), "party mode did not start");
            return if no {
                "Beklager, jeg fikk ikke startet festmodus."
            } else {
                "Sorry, I couldn't start party mode."
            }
            .into();
        }
        match (pace, no) {
            (Pace::Relaxed, false) => "Okay, relaxed party mode is on.",
            (Pace::Normal, false) => "Okay, party mode is on.",
            (Pace::Fast, false) => "Okay, fast party mode is on.",
            (Pace::Relaxed, true) => "Ok, rolig festmodus er på.",
            (Pace::Normal, true) => "Ok, festmodus er på.",
            (Pace::Fast, true) => "Ok, rask festmodus er på.",
        }
        .into()
    }

    /// Ends party mode and puts the lights back once the stream has let go of them. Whether it was on.
    fn party_off(&mut self) -> bool {
        if self.config.party.mode().is_none() {
            return false;
        }
        self.config.party.stop();
        if let Some((url, token)) = self.config.home_assistant.clone() {
            // On a thread of its own: the bridge lets go of the lights after the stream's last frame.
            let restore = std::thread::Builder::new().name("party-restore".into()).spawn(move || {
                std::thread::sleep(Duration::from_secs(1));
                if let Err(error) = house::party_restore(&HomeAssistant::new(&url, &token)) {
                    warn!(error = format!("{error:#}"), "party mode: could not put the lights back");
                }
            });
            if let Err(error) = restore {
                warn!(%error, "party mode: could not put the lights back");
            }
        }
        true
    }

    /// Every light red, the volume up and the lockdown music on. Just for fun: nothing is locked.
    fn lockdown(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        // Its red lights and the party's stream would fight over the same lights.
        self.party_off();
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        match house::lockdown(ha) {
            Ok(red) => {
                info!(red = red.len(), "lockdown");
                let blinking = self.alarm.as_ref().is_some_and(|a| !a.thread.is_finished());
                if !blinking && !red.is_empty() {
                    match Alarm::start(ha.detached(), red) {
                        Ok(alarm) => self.alarm = Some(alarm),
                        Err(error) => warn!(%error, "could not start the lockdown's blink"),
                    }
                }
            }
            Err(error) => return unreachable(&error, no),
        }
        let mut level = f64::from(self.config.lockdown_volume) / 100.0;
        if let Ok(entity) = self.player.check(ha) {
            let volume = ha.state(&entity).ok().and_then(|s| s["attributes"]["volume_level"].as_f64());
            // Music already playing louder than the setting stays as loud: a lockdown never quietens it.
            level = level.max(volume.unwrap_or(0.0));
            // A second lockdown keeps the volume from before the first.
            self.lockdown_volume = self.lockdown_volume.or(volume);
        }
        self.lockdown_screen();
        // The lockdown's music replaces what was paused for the request.
        self.hushed = None;
        let music = MusicCommand::Song(self.config.lockdown_music.clone());
        self.after_answer = Some((music, Some(level)));
        self.music_under_answer = true;
        // When it began, so a TV that opens the screen later still counts from the start.
        let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        if !self.screen.showing("lockdown") {
            self.show("lockdown", serde_json::json!({ "since": since }), None);
        }
        if no {
            return "Nedstengning startet. Huset er forseglet.".into();
        }
        let threat = house::LOCKDOWN_THREATS[self.threat_turn % house::LOCKDOWN_THREATS.len()];
        self.threat_turn += 1;
        format!("Lockdown initiated. The house is sealed. {threat}")
    }

    /// Puts the lockdown's screen on a TV that is on and not showing it yet. On a thread of its own:
    /// the TV takes a while to answer, and the answer need not wait for it.
    fn lockdown_screen(&mut self) {
        let Some(url) = self.config.screen_url.clone() else { return };
        let Some(ha) = &self.home_assistant else { return };
        if self.lockdown_tv.is_some() || self.screen.watched() {
            return;
        }
        let (entity, source) = match self.tv.on_source(ha) {
            Ok(Some(on)) => on,
            Ok(None) => return,
            Err(error) => return warn!(error = format!("{error:#}"), "no tv for the lockdown"),
        };
        let (tv_ha, tv_entity) = (ha.detached(), entity.clone());
        let opened = std::thread::Builder::new().name("tv".into()).spawn(move || {
            if let Err(error) = tv::open_url(&tv_ha, &tv_entity, &url) {
                warn!(error = format!("{error:#}"), "could not open the lockdown on the tv");
            }
        });
        match opened {
            Ok(_) => self.lockdown_tv = Some((entity, source)),
            Err(error) => warn!(%error, "could not start the tv thread"),
        }
    }

    /// Starts or stops the music the last answer was about, and sets the volume, now that the
    /// answer has played. Volume first for new music, last when stopping, so neither is heard at
    /// the wrong level.
    fn run_after_answer(&mut self) {
        self.after_answer_at = None;
        self.music_under_answer = false;
        let Some((music, volume)) = self.after_answer.take() else { return };
        let Some(ha) = &self.home_assistant else { return };
        let set_volume = |player: &mut Player| {
            let Some(level) = volume else { return };
            let set = player.check(ha).and_then(|entity| {
                ha.service(
                    "media_player",
                    "volume_set",
                    serde_json::json!({ "entity_id": entity, "volume_level": ha::volume_level(level * 100.0) }),
                )
            });
            if let Err(error) = set {
                warn!(error = format!("{error:#}"), "could not set the music's volume");
            }
        };
        let stopping = music == MusicCommand::Stop;
        if !stopping {
            set_volume(&mut self.player);
        }
        self.player.run(Some(ha), &music, Lang::English);
        if stopping {
            set_volume(&mut self.player);
        }
        info!(?music, ?volume, "music after the answer");
    }

    fn stand_down(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        // Stopped first: a step after the lights are restored would turn them red again.
        if let Some(alarm) = self.alarm.take() {
            alarm.stop();
        }
        let restored = match house::stand_down(ha) {
            Ok(restored) => restored,
            Err(error) => return unreachable(&error, no),
        };
        let volume = self.lockdown_volume.take();
        if !restored && volume.is_none() {
            return if no { "Det er ingen nedstengning å avslutte." } else { "There is no lockdown to end." }.into();
        }
        // The music was paused for the request: it stays paused under the answer, then stops.
        self.hushed = None;
        self.after_answer = Some((MusicCommand::Stop, volume));
        self.music_under_answer = false;
        info!(restored, ?volume, "lockdown over");
        // Back to what the TV showed, unless someone has since switched it to something else.
        if let Some((entity, Some(source))) = self.lockdown_tv.take().filter(|_| self.screen.watched()) {
            let tv_ha = ha.detached();
            let back = std::thread::Builder::new().name("tv".into()).spawn(move || {
                if let Err(error) = tv::select_source(&tv_ha, &entity, &source) {
                    warn!(error = format!("{error:#}"), "could not put the tv back");
                }
            });
            if let Err(error) = back {
                warn!(%error, "could not start the tv thread");
            }
        }
        if self.screen.showing("lockdown") {
            self.hide();
        }
        if no { "Avblåst. Alt er som før." } else { "Standing down. Everything is back as it was." }.into()
    }

    fn house(&mut self, command: &HouseCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let name = self.config.wake.name().to_owned();
        let here = self.room.clone();
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        match house_answer(ha, command, here.as_deref(), lang, &name) {
            Ok(answer) => {
                info!(?command, "house");
                answer
            }
            Err(error) => unreachable(&error, no),
        }
    }

    /// Leaving, coming home and bedtime. Lights a routine switched count as the last lights, so
    /// "undo" or "turn them back on" reverses it.
    fn routine(&mut self, routine: Routine, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let here = self.room.clone();
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let mut answer = match routine_answer(ha, routine, here.as_deref(), &self.config, lang) {
            Ok((answer, switched)) => {
                if let Some((target, on)) = switched {
                    self.last_lights = Some((target, on, Instant::now()));
                }
                answer
            }
            Err(error) => return unreachable(&error, no),
        };
        if let (Routine::Home, Some(music)) = (routine, &self.config.home_music) {
            let command = if music.eq_ignore_ascii_case("liked") {
                MusicCommand::Liked
            } else {
                MusicCommand::Playlist(music.clone())
            };
            let started = self.player.run(self.home_assistant.as_ref(), &command, lang);
            answer = format!("{answer} {started}");
        }
        info!(?routine, answer, "routine");
        if routine == Routine::Home {
            self.show("welcome", serde_json::json!({ "text": answer }), Some(SCREEN_MOMENT));
        }
        answer
    }

    /// The TV screen with `module` on it, for pages showing it now and any that open later.
    fn show(&mut self, module: &str, data: serde_json::Value, ttl: Option<Duration>) {
        let message = self.screen.show(module, data, ttl);
        self.emit(AssistantEvent::Show(Some(message)));
    }

    fn hide(&mut self) {
        self.screen.hide();
        self.emit(AssistantEvent::Show(None));
    }

    /// "Show the weather on the TV": the data on the screen, and the screen opened in the TV's
    /// browser when no page shows it yet.
    fn show_screen(&mut self, screen: tv::Screen, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let shown = match screen {
            tv::Screen::Jarvis => {
                self.hide();
                Ok(if no { "Ok, her er jeg." } else { "Okay, I'm on the TV." })
            }
            tv::Screen::Weather => self.screen_weather(no),
            tv::Screen::Timers => self.screen_timers(no),
            tv::Screen::Music => self.screen_music(no),
            tv::Screen::Departures => self.screen_departures(no),
        };
        let done = match shown {
            Ok(done) => done,
            Err(answer) => return answer,
        };
        if self.screen.watched() {
            return done.into();
        }
        let Some(url) = self.config.screen_url.clone() else {
            return if no {
                "TV-skjermen er ikke satt opp. Den trenger innstillingen screen-url."
            } else {
                "The TV screen isn't set up. It needs the screen-url setting."
            }
            .into();
        };
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        match self.tv.open_page(ha, &url) {
            Ok(()) => done.into(),
            Err(error) => {
                warn!(error = format!("{error:#}"), "could not open the screen on the tv");
                if no {
                    "Beklager, jeg fikk ikke åpnet skjermen på TV-en."
                } else {
                    "Sorry, I couldn't open the screen on the TV."
                }
                .into()
            }
        }
    }

    fn screen_weather(&mut self, no: bool) -> Result<&'static str, String> {
        let Some(home) = self.home_location.clone() else {
            return Err(if no {
                "Jeg vet ikke hvor hjemme er. Den trenger innstillingen address."
            } else {
                "I don't know where home is. It needs the address setting."
            }
            .into());
        };
        match self.weather.screen(&home) {
            Ok(Some(data)) => {
                self.show("weather", data, Some(SCREEN_DATA));
                Ok(if no { "Ok, været er på TV-en." } else { "Okay, the weather is on the TV." })
            }
            Ok(None) => Err(if no { "Jeg fikk ingen værmelding." } else { "I got no forecast." }.into()),
            Err(error) => {
                warn!(error = format!("{error:#}"), "weather for the screen failed");
                Err(if no {
                    "Beklager, jeg får ikke hentet været nå."
                } else {
                    "Sorry, I couldn't get the forecast right now."
                }
                .into())
            }
        }
    }

    fn screen_timers(&mut self, no: bool) -> Result<&'static str, String> {
        let now = Instant::now();
        let timers: Vec<serde_json::Value> = self
            .timers
            .list()
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": format!("timer-{}", t.id),
                    "name": t.name.clone().or_else(|| t.reminder.clone()).unwrap_or_default(),
                    "left": t.left(now).as_secs_f32().ceil() as u32,
                    "total": t.total.as_secs(),
                    "active": t.is_running(),
                })
            })
            .collect();
        if timers.is_empty() {
            return Err(if no { "Ingen tidtakere går nå." } else { "There are no timers running." }.into());
        }
        // Until the last timer is gone: the page counts down and drops them as they end.
        self.show("timers", serde_json::json!({ "timers": timers }), None);
        Ok(if no { "Ok, tidtakerne er på TV-en." } else { "Okay, the timers are on the TV." })
    }

    fn screen_music(&mut self, no: bool) -> Result<&'static str, String> {
        let not_connected = || -> String {
            if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }.into()
        };
        let Some(ha) = &self.home_assistant else { return Err(not_connected()) };
        let state = self.player.check(ha).and_then(|entity| ha.state(&entity));
        let state = match state {
            Ok(state) => state,
            Err(error) => {
                warn!(error = format!("{error:#}"), "music for the screen failed");
                return Err(if no {
                    "Beklager, jeg finner ikke musikken nå."
                } else {
                    "Sorry, I couldn't find the music right now."
                }
                .into());
            }
        };
        let a = &state["attributes"];
        if !["playing", "paused"].contains(&state["state"].as_str().unwrap_or_default()) || a["media_title"].is_null() {
            return Err(if no { "Ingenting spilles nå." } else { "Nothing is playing." }.into());
        }
        let data = serde_json::json!({
            "title": a["media_title"],
            "artist": a["media_artist"],
            "album": a["media_album_name"],
            "position": a["media_position"],
            "updated": a["media_position_updated_at"],
            "duration": a["media_duration"],
            "playing": state["state"] == "playing",
        });
        self.show("music", data, Some(SCREEN_MUSIC));
        Ok(if no { "Ok, musikken er på TV-en." } else { "Okay, the music is on the TV." })
    }

    fn screen_departures(&mut self, no: bool) -> Result<&'static str, String> {
        if !self.transit.is_configured() {
            return Err(if no {
                "Jeg vet ikke hvor hjemme er. Den trenger innstillingen address."
            } else {
                "I don't know where home is. It needs the address setting."
            }
            .into());
        }
        match self.transit.board() {
            Ok(Some(data)) => {
                self.show("departures", data, Some(SCREEN_DATA));
                Ok(if no { "Ok, avgangene er på TV-en." } else { "Okay, the departures are on the TV." })
            }
            Ok(None) => {
                Err(if no { "Jeg fant ingen avganger i nærheten." } else { "I found no departures nearby." }.into())
            }
            Err(error) => {
                warn!(error = format!("{error:#}"), "departures for the screen failed");
                Err(if no {
                    "Beklager, jeg får ikke hentet avgangene nå."
                } else {
                    "Sorry, I couldn't get the departures right now."
                }
                .into())
            }
        }
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
            None if mentions_everywhere(request) => {
                let lights = ha.lights().map_err(|error| unreachable(&error, no))?;
                // Each lamp, not the room groups as well, so nothing is switched twice.
                let ids = lights.iter().filter(|l| !l.group && !l.status_led()).map(|l| l.id.clone()).collect();
                let label = if no { "huset" } else { "house" }.to_owned();
                Ok(Some(Target { label, ids, area: true }))
            }
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
        match self.news.briefing(lang) {
            Ok(headlines) => {
                self.live.extend(headlines.live);
                parts.push(headlines.text);
            }
            Err(error) => warn!(error = format!("{error:#}"), "no news for the briefing"),
        }
        parts.join(" ")
    }

    fn distance(&mut self, query: &voice_assistant::distance::Query, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let from = match &query.from {
            Some(name) => self.weather.find(name),
            None => Ok(self.home_location.clone()),
        };
        let places = from.and_then(|from| Ok((from, self.weather.find(&query.to)?)));
        match places {
            Ok((Some(from), Some(to))) => voice_assistant::distance::answer(&from, &to, lang),
            Ok((None, _)) if query.from.is_none() && no => "Hvor fra? Si for eksempel fra Oslo til Bergen.".into(),
            Ok((None, _)) if query.from.is_none() => "From where? Say, for example, from Oslo to Bergen.".into(),
            Ok(_) if no => "Beklager, jeg fant ikke det stedet.".into(),
            Ok(_) => "Sorry, I couldn't find that place.".into(),
            Err(error) => {
                warn!(error = format!("{error:#}"), "place lookup failed");
                if no {
                    "Beklager, jeg får ikke slått opp stedet nå."
                } else {
                    "Sorry, I couldn't look up the place right now."
                }
                .into()
            }
        }
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

    /// `brief`: one of several items said one after the other.
    fn shopping_list(&mut self, command: ListCommand, lang: Lang, brief: bool) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let adding = matches!(command, ListCommand::Add(_));
        self.list_open = adding.then(Instant::now);
        self.shopping.brief = brief;
        match self.shopping.run(ha, &command, lang) {
            Ok((answer, change)) => {
                info!(?command, ?change, "shopping list");
                let added = matches!(change, Some(Change::Added(_)));
                if let Some(change) = change {
                    self.last_list = Some((change, Instant::now()));
                }
                // The first item of a few: say that more may follow without "add".
                match (added && !brief, no) {
                    (true, false) => format!("{answer} Anything else?"),
                    (true, true) => format!("{answer} Noe mer?"),
                    (false, _) => answer,
                }
            }
            Err(error) => unreachable(&error, no),
        }
    }

    /// The items in a reply while the list is open. Said alone, a Norwegian item is better heard by
    /// Whisper told the language, when it is set up; otherwise the listener's words.
    fn items_heard(&mut self, text: &str) -> Vec<String> {
        let norwegian = self.config.norwegian_groceries.as_ref().filter(|_| !self.request_audio.is_empty());
        if let Some(norwegian) = norwegian {
            let start = Instant::now();
            let heard = norwegian.transcribe(&self.request_audio);
            info!(heard, english = text, took = ?start.elapsed(), "items heard in norwegian");
            let items = voice_assistant::shopping::items_of(&heard);
            if !items.is_empty() {
                return items;
            }
        }
        voice_assistant::shopping::items_of(text)
    }

    fn list_is_open(&self) -> bool {
        self.list_open.is_some_and(|at| at.elapsed() < LIST_OPEN)
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
        let places = house::rooms_lit(&on);
        let list = house::join(&places, lang);
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
        self.show("scene", serde_json::json!({ "name": scene.name }), Some(SCREEN_MOMENT));
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
    /// Whom the next answers address, by how the wake word's voice sounded. Follow-ups keep it.
    fn address(&mut self, gender: Option<Gender>) {
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

    /// Rings for timers that ran out and speaks what they were for.
    fn ring_due_timers(&mut self) {
        for (timer, notice) in self.timers.due(Instant::now()) {
            let text = timer::finished_text(&timer);
            info!(text, "timer done");
            self.emit(AssistantEvent::Timer(notice));
            let chime = if timer.is_reminder { tts::reminder_chime() } else { tts::alarm_chime() };
            self.announce(&text, chime, timer.lang, !timer.is_reminder);
        }
    }

    /// Speaks outside a conversation, after a chime. The audio also goes to satellites as an
    /// announcement.
    fn announce(&mut self, text: &str, chime: Vec<i16>, lang: Lang, timer_ring: bool) {
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

    /// Music Assistant's player is playing.
    fn music_playing(&mut self) -> bool {
        let Some(ha) = &self.home_assistant else { return false };
        let Ok(entity) = self.player.check(ha) else { return false };
        ha.state(&entity).is_ok_and(|s| s["state"] == "playing")
    }

    /// Pauses the music for the request that follows a wake word.
    fn hush(&mut self) {
        if let Some(hushed) = &mut self.hushed {
            hushed.last_heard = Instant::now();
            return;
        }
        let Some(ha) = &mut self.home_assistant else { return };
        // The Sonos itself can be playing while its Music Assistant player says idle, so both count.
        let music = self.player.check(ha).ok().into_iter().collect::<Vec<_>>();
        let players = match ha.playing(self.room.as_deref(), &music) {
            Ok(players) => players,
            Err(error) => return warn!(error = format!("{error:#}"), "could not see what is playing"),
        };
        let players = players_to_hush(players, &music);
        if players.is_empty() {
            return;
        }
        let ids: Vec<&str> = players.iter().map(|p| p.id.as_str()).collect();
        if let Err(error) = ha.send("media_player", "media_pause", serde_json::json!({ "entity_id": ids })) {
            return warn!(error = format!("{error:#}"), "could not pause the music");
        }
        info!(players = ?ids, "music paused for a request");
        let now = Instant::now();
        self.hushed = Some(Hushed { players, at: now, last_heard: now });
    }

    fn unhush(&mut self) {
        let Some(hushed) = self.hushed.take() else { return };
        let Some(ha) = &self.home_assistant else { return };
        // A speaker's own player resumes whatever its transport holds by then (a radio station, a
        // stale stream), so only one still holding what was paused plays on.
        let ids: Vec<&str> = hushed
            .players
            .iter()
            .filter(|p| {
                !p.sonos || ha.state(&p.id).is_ok_and(|s| s["attributes"]["media_content_id"] == p.content.as_str())
            })
            .map(|p| p.id.as_str())
            .collect();
        if ids.len() < hushed.players.len() {
            info!(players = ?hushed.players.iter().map(|p| &p.id).collect::<Vec<_>>(), "a paused speaker changed source; not resuming it");
        }
        if ids.is_empty() {
            return;
        }
        match ha.send("media_player", "media_play", serde_json::json!({ "entity_id": ids })) {
            Ok(()) => info!(players = ?ids, "music resumed after a request"),
            Err(error) => warn!(error = format!("{error:#}"), "could not resume the music"),
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

/// Checks the safety sensors every few seconds and hands new warnings to the worker, which speaks
/// them even while listening is paused.
fn watch_alerts(ha: HomeAssistant, jobs: mpsc::Sender<Job>) {
    let mut watch = Watch::default();
    let mut failing = false;
    loop {
        std::thread::sleep(ALERT_POLL);
        match alerts::active(&ha) {
            Ok(active) => {
                failing = false;
                for alert in watch.due(&active, Instant::now()) {
                    info!(id = alert.id, "safety sensor on");
                    if jobs.send(Job::Alert(alerts::sentence(alert, Lang::English))).is_err() {
                        return;
                    }
                }
            }
            // Once per outage: Home Assistant restarts now and then.
            Err(error) if !failing => {
                failing = true;
                warn!(error = format!("{error:#}"), "could not check the safety sensors");
            }
            Err(_) => {}
        }
    }
}

fn house_answer(
    ha: &mut HomeAssistant,
    command: &HouseCommand,
    here: Option<&str>,
    lang: Lang,
    name: &str,
) -> anyhow::Result<String> {
    use serde_json::json;
    let no = lang == Lang::Norwegian;
    let say = |english: &str, norwegian: &str| if no { norwegian.to_owned() } else { english.to_owned() };
    Ok(match command {
        HouseCommand::Status => {
            let alerts = alerts::active(ha)?;
            let lights: Vec<Light> = ha.lights_on()?.into_iter().filter(|l| !l.name.ends_with(" LED")).collect();
            house::status_answer(&alerts, &lights, &house::snapshot(ha)?, lang)
        }
        HouseCommand::Sun(event) => house::sun_answer(&ha.state("sun.sun")?, *event, lang)?,
        HouseCommand::FindPhone(who) => {
            let phones = house::phones(ha)?;
            let picked = house::pick_phones(&phones, who.as_deref());
            for phone in &picked {
                house::ring(ha, phone, name)?;
                info!(phone = phone.id, "ringing phone");
            }
            house::rang_answer(&picked, who.as_deref(), lang)
        }
        HouseCommand::PhoneBattery(who) => {
            let phones = house::phones(ha)?;
            house::battery_answer(&house::pick_phones(&phones, who.as_deref()), who.as_deref(), lang)
        }
        HouseCommand::GuestWifi(switch) => {
            let Some(wifi) = house::entities(ha, "switch", "guest")?.into_iter().next() else {
                return Ok(say(
                    "I can't find a guest Wi-Fi in Home Assistant.",
                    "Jeg finner ikke gjestenettet i Home Assistant.",
                ));
            };
            match switch {
                Switch::On | Switch::Off => {
                    let on = *switch == Switch::On;
                    ha.service("switch", if on { "turn_on" } else { "turn_off" }, json!({ "entity_id": wifi.id }))?;
                    match on {
                        true => say("Okay, the guest Wi-Fi is on.", "Ok, gjestenettet er på."),
                        false => say("Okay, the guest Wi-Fi is off.", "Ok, gjestenettet er av."),
                    }
                }
                Switch::Status if wifi.state != "on" => say("The guest Wi-Fi is off.", "Gjestenettet er av."),
                Switch::Status => {
                    let clients = match (wifi.clients.parse::<u32>(), no) {
                        (Ok(0), false) => " Nobody is connected.".to_owned(),
                        (Ok(0), true) => " Ingen er koblet til.".to_owned(),
                        (Ok(1), false) => " One device is connected.".to_owned(),
                        (Ok(1), true) => " Én enhet er koblet til.".to_owned(),
                        (Ok(n), false) => format!(" {n} devices are connected."),
                        (Ok(n), true) => format!(" {n} enheter er koblet til."),
                        (Err(_), _) => String::new(),
                    };
                    say("The guest Wi-Fi is on.", "Gjestenettet er på.") + &clients
                }
            }
        }
        HouseCommand::Camera(switch) => {
            let privacy = house::entities(ha, "switch", "privacy")?;
            if privacy.is_empty() {
                return Ok(say(
                    "I can't find a camera with a privacy mode.",
                    "Jeg finner ikke noe kamera med personvernmodus.",
                ));
            }
            let ids: Vec<&str> = privacy.iter().map(|p| p.id.as_str()).collect();
            match switch {
                Switch::On => {
                    ha.service("switch", "turn_off", json!({ "entity_id": ids }))?;
                    say("Okay, the camera is back on.", "Ok, kameraet er på igjen.")
                }
                Switch::Off => {
                    ha.service("switch", "turn_on", json!({ "entity_id": ids }))?;
                    say(
                        "Okay, privacy mode is on. The camera isn't recording.",
                        "Ok, personvernmodus er på. Kameraet tar ikke opp.",
                    )
                }
                Switch::Status if privacy.iter().any(|p| p.state == "on") => {
                    say("The camera is in privacy mode.", "Kameraet er i personvernmodus.")
                }
                Switch::Status if house::entities(ha, "camera", "")?.iter().any(|c| c.state == "recording") => {
                    say("The camera is recording.", "Kameraet tar opp.")
                }
                Switch::Status => say("The camera is on, but not recording.", "Kameraet er på, men tar ikke opp."),
            }
        }
        HouseCommand::Alarm(switch) => {
            let Some(panel) = house::entities(ha, "alarm_control_panel", "")?.into_iter().next() else {
                return Ok(say("There's no alarm in Home Assistant.", "Det er ingen alarm i Home Assistant."));
            };
            match switch {
                Switch::Off => say(
                    "I won't disarm the alarm by voice; anyone at the door could ask. Use the app instead.",
                    "Jeg slår ikke av alarmen med stemmen; hvem som helst ved døra kunne spurt. Bruk appen.",
                ),
                Switch::Status => house::alarm_sentence(&panel.state, lang)
                    .unwrap_or_else(|| say("I can't tell whether the alarm is on.", "Jeg vet ikke om alarmen er på.")),
                Switch::On if panel.state.starts_with("armed") => {
                    say("The alarm is already armed.", "Alarmen er allerede på.")
                }
                Switch::On => {
                    ha.service("alarm_control_panel", "alarm_arm_away", json!({ "entity_id": panel.id }))?;
                    info!(panel = panel.id, "alarm armed");
                    say("Okay, the alarm is armed.", "Ok, alarmen er på.")
                }
            }
        }
        HouseCommand::Climate(place, reading) => {
            house::climate_answer(&house::climate_readings(ha)?, place, *reading, here, lang)
        }
    })
}

/// Whether any lamp of `ids` is on, a group through its own state.
fn plan_lit(lamps: &[plan::Lamp], ids: &[String]) -> bool {
    lamps.iter().any(|l| l.on && ids.contains(&l.id))
}

/// Bedtime light colour: warm white.
const BEDTIME_KELVIN: u32 = 2700;

/// What bedtime leaves on.
fn in_bedroom(light: &Light) -> bool {
    ha::is_room(&light.area, "bedroom") || ha::is_room(&light.name, "bedroom")
}

/// The bedroom lights that are on which bedtime dims. A Hue room is also its own lights: the
/// lights, or the room when that is all there is.
fn bedtime_dimmed(bedroom: &[Light]) -> Vec<&str> {
    let single: Vec<&Light> = bedroom.iter().filter(|l| !l.group).collect();
    if single.is_empty() { bedroom.iter().collect() } else { single }.iter().map(|l| l.id.as_str()).collect()
}

/// The lights coming home switches on, when none of them is on yet.
fn home_target(ha: &mut HomeAssistant, config: &AssistantConfig, here: Option<&str>) -> anyhow::Result<Option<Target>> {
    let room = match &config.home_room {
        Some(room) => Some(room.as_str()),
        None => here.filter(|_| ha.state("sun.sun").is_ok_and(|s| s["state"] == "below_horizon")),
    };
    Ok(room.map(|room| ha.find(room)).transpose()?.flatten())
}

/// A routine's answer, and the lights it switched for "undo".
fn routine_answer(
    ha: &mut HomeAssistant,
    routine: Routine,
    here: Option<&str>,
    config: &AssistantConfig,
    lang: Lang,
) -> anyhow::Result<(String, Option<(Target, bool)>)> {
    let no = lang == Lang::Norwegian;
    let mut parts = Vec::new();
    let mut switched_lights = None;
    match routine {
        Routine::Leaving | Routine::Bedtime => {
            let bedtime = routine == Routine::Bedtime;
            let keep = |l: &Light| bedtime && in_bedroom(l);
            let switched = house::everything_off(ha, keep)?;
            let except = if bedtime && !switched.lights.is_empty() {
                Some(if no { "soverommet" } else { "bedroom" })
            } else {
                None
            };
            parts.push(house::switched_answer(&switched, except, lang));
            if bedtime {
                let bedroom: Vec<Light> = ha.lights_on()?.into_iter().filter(|l| keep(l)).collect();
                let ids = bedtime_dimmed(&bedroom);
                if !ids.is_empty() {
                    let data = serde_json::json!({
                        "entity_id": ids,
                        "brightness_pct": config.bedtime_light,
                        "color_temp_kelvin": BEDTIME_KELVIN,
                    });
                    ha.send("light", "turn_on", data)?;
                    parts.push(
                        if no { "Lyset på soverommet er dempet." } else { "The bedroom lights are dimmed." }.into(),
                    );
                }
            }
            if !switched.lights.is_empty() {
                let ids = switched.lights.iter().map(|l| l.id.clone()).collect();
                let label = if no { "huset" } else { "house" }.to_owned();
                switched_lights = Some((Target { label, ids, area: true }, false));
            }
            parts.extend(alerts::active(ha)?.iter().map(|a| alerts::sentence(a, lang)));
            if !bedtime && house::snapshot(ha)?.alarm == "disarmed" {
                parts.push(
                    if no {
                        "Alarmen er av. Si slå på alarmen hvis du vil."
                    } else {
                        "The alarm is off. Say arm the alarm to set it."
                    }
                    .into(),
                );
            }
            parts.push(
                match (bedtime, no) {
                    (false, false) => "See you later.",
                    (false, true) => "Ha det bra.",
                    (true, false) => "Good night.",
                    (true, true) => "God natt.",
                }
                .into(),
            );
        }
        Routine::Lockdown | Routine::StandDown => unreachable!("the worker runs these itself"),
        Routine::Home => {
            parts.push(if no { "Velkommen hjem." } else { "Welcome home." }.into());
            if let Some(target) = home_target(ha, config, here)? {
                let on = ha.lights_on()?;
                if !on.iter().any(|l| target.ids.contains(&l.id)) {
                    ha.set(&target, true)?;
                    let label = target.label.to_lowercase();
                    parts.push(if no {
                        format!("Jeg har skrudd på lyset i {label}.")
                    } else {
                        format!("I've turned on the {label} lights.")
                    });
                    switched_lights = Some((target, true));
                }
            }
            parts.extend(alerts::active(ha)?.iter().map(|a| alerts::sentence(a, lang)));
            if house::snapshot(ha)?.alarm.starts_with("armed") {
                parts.push(if no { "Alarmen er fortsatt på." } else { "The alarm is still armed." }.into());
            }
        }
    }
    Ok((parts.join(" "), switched_lights))
}

/// "Stop", "stop it": never mind, or the music when it plays.
fn bare_stop(request: &str) -> bool {
    let words: Vec<String> =
        request.split_whitespace().map(voice_assistant::dialog::normalize).filter(|w| !w.is_empty()).collect();
    matches!(words.as_slice(), [w] | [w, _] if ["stop", "stopp"].contains(&w.as_str()))
        && intent::parse(request) == Intent::Cancel
}

/// How requests open, after any "hey", "please" or the name: a command or a question. Talk from
/// the TV or the room mostly opens otherwise ("I'm...", "That's...", "The...").
const REQUEST_OPENINGS: &[&str] = &[
    "turn",
    "switch",
    "set",
    "start",
    "stop",
    "pause",
    "play",
    "resume",
    "cancel",
    "add",
    "remove",
    "put",
    "show",
    "showed",
    "tell",
    "give",
    "make",
    "open",
    "close",
    "lock",
    "unlock",
    "dim",
    "brighten",
    "raise",
    "lower",
    "increase",
    "decrease",
    "mute",
    "unmute",
    "skip",
    "next",
    "previous",
    "remind",
    "find",
    "ring",
    "call",
    "check",
    "read",
    "arm",
    "disarm",
    "activate",
    "deactivate",
    "enable",
    "disable",
    "shuffle",
    "repeat",
    "louder",
    "quieter",
    "volume",
    "lights",
    "light",
    "timer",
    "music",
    "what",
    "what's",
    "whats",
    "when",
    "where",
    "who",
    "who's",
    "whos",
    "how",
    "how's",
    "hows",
    "why",
    "which",
    "is",
    "are",
    "do",
    "does",
    "can",
    "could",
    "would",
    "will",
    "should",
    "any",
    "slå",
    "skru",
    "sett",
    "spill",
    "stopp",
    "start",
    "vis",
    "fortell",
    "legg",
    "hva",
    "hvor",
    "hvem",
    "hvordan",
    "hvorfor",
    "når",
    "kan",
    "er",
];
const REQUEST_FILLER: &[&str] =
    &["hey", "hi", "ok", "okay", "please", "so", "um", "uh", "oh", "and", "now", "just", "hei", "kjære"];

/// Words in a follow-up without the name, past which it has to open like a request: a long
/// sentence that does not is the TV or the room.
const FOLLOW_UP_MOST_WORDS: usize = 8;

/// Speech in the follow-up window that is too long and does not open like a request.
fn room_talk(text: &str) -> bool {
    text.split_whitespace().count() > FOLLOW_UP_MOST_WORDS && !sounds_like_a_request(text)
}

/// Whether unrecognised speech opens like a request at all.
fn sounds_like_a_request(text: &str) -> bool {
    text.split_whitespace()
        .map(voice_assistant::dialog::normalize)
        .find(|w| !w.is_empty() && !REQUEST_FILLER.contains(&w.as_str()))
        .is_some_and(|w| REQUEST_OPENINGS.contains(&w.as_str()))
}

/// Whisper hears "pause" and "play" after the wake word as "please", "nice", "post": a short request
/// that ends in "music" and is nothing else means pausing it when it was playing, else playing it.
/// A prompt with the command words fixes some of these but makes up commands from room noise.
fn music_by_sound(request: &str, playing: bool) -> Option<String> {
    let words: Vec<String> =
        request.split_whitespace().map(voice_assistant::dialog::normalize).filter(|w| !w.is_empty()).collect();
    // "Stop" over music means the music, not "never mind".
    if bare_stop(request) && playing {
        return Some("pause the music".into());
    }
    // Over music, a short request with a percentage and no light is the volume: "put the percentage
    // at 70 percent", "trop with the sign at 50 percent" (drop the sound).
    if playing && words.len() <= 8 && intent::parse(request) == Intent::Unknown {
        let percent = words.windows(2).find_map(|p| {
            let n = p[0].parse::<u8>().ok().filter(|n| *n <= 100)?;
            ["percent", "prosent"].contains(&p[1].as_str()).then_some(n)
        });
        if let Some(n) = percent {
            return Some(format!("set the volume to {n}"));
        }
    }
    // "Play my liked songs" comes out as "claim a light song", "they're my like songs", "clean
    // my life's songs": the play is lost under the wake word's end and "liked" sounds like these.
    let songs = words.last().is_some_and(|w| w == "song" || w == "songs");
    let liked = words
        .iter()
        .any(|w| ["like", "liked", "likes", "light", "lights", "life", "life's", "lifes"].contains(&w.as_str()));
    if songs && liked && words.len() <= 5 && intent::parse(request) == Intent::Unknown {
        return Some("play my liked songs".into());
    }
    let music = words.last().is_some_and(|w| ["music", "musikk", "musikken"].contains(&w.as_str()));
    (music && words.len() <= 4 && intent::parse(request) == Intent::Unknown)
        .then(|| if playing { "pause the music" } else { "play some music" }.into())
}

/// An action whose result is heard (the music stops, the next song starts) needs no words, which
/// would only arrive a second or two later from the speaker.
fn heard_by_itself(intent: &Intent) -> bool {
    matches!(
        intent,
        Intent::Music(
            MusicCommand::Pause
                | MusicCommand::Resume
                | MusicCommand::Stop
                | MusicCommand::Mute(_)
                | MusicCommand::Next
                | MusicCommand::Previous
                | MusicCommand::Liked
                | MusicCommand::Playlist(_)
                | MusicCommand::Song(_)
                | MusicCommand::Artist(_)
                | MusicCommand::Album(_)
        ) | Intent::Tv(TvCommand::Pause | TvCommand::Resume)
    )
}

/// The players to pause for a request. While Music Assistant plays on a speaker, the speaker's own
/// player reports playing too; pausing and resuming it as well races Music Assistant's stream and
/// can bring back the speaker's last radio station instead.
fn players_to_hush(players: Vec<Playing>, music: &[String]) -> Vec<Playing> {
    let assistant_plays = players.iter().any(|p| music.contains(&p.id));
    players.into_iter().filter(|p| !(assistant_plays && p.sonos)).collect()
}

/// Commands that nothing said after them could change, so a draft that is one is acted on. Not
/// resume: "play" is also the start of "play Careless Whisper"; not louder or quieter: "increase
/// the sound" may go on "by 25%".
fn complete_at_once(intent: &Intent) -> bool {
    match intent {
        // "Turn off the kitchen lights", not "turn off the lights" (may go on "in the kitchen").
        Intent::Lights { target, .. } => names_light_last(target),
        // A level or colour ends the request; "dim the kitchen" may go on "to 30 percent".
        Intent::LightLevel { level: LightLevel::Percent(_) | LightLevel::Color(_), target } => {
            ha::names_something(target)
        }
        // Small talk is a whole short phrase; a bare "hey" is often a request still to come.
        Intent::SmallTalk(chat) => *chat != voice_assistant::smalltalk::Chat::Hello,
        _ => complete_command(intent),
    }
}

/// Whether a light request ends on what it switches: "the kitchen lights", "the kitchen".
fn names_light_last(target: &str) -> bool {
    let words: Vec<String> = target.split_whitespace().map(voice_assistant::dialog::normalize).collect();
    let last = words.last().map(String::as_str).unwrap_or_default();
    ha::names_something(target) && (["light", "lights", "lamp", "lamps"].contains(&last) || ha::names_only_room(&words))
}

fn complete_command(intent: &Intent) -> bool {
    matches!(
        intent,
        Intent::Cancel
            | Intent::Music(
                MusicCommand::Pause
                    | MusicCommand::Stop
                    | MusicCommand::Mute(_)
                    | MusicCommand::NowPlaying
                    | MusicCommand::Shuffle(_)
                    | MusicCommand::Repeat(_)
                    | MusicCommand::Next
                    | MusicCommand::Previous
                    | MusicCommand::VolumeBy(_)
                    | MusicCommand::VolumeTo(_)
            )
            | Intent::Tv(
                TvCommand::Pause
                    | TvCommand::Resume
                    | TvCommand::Power(_)
                    | TvCommand::Mute(_)
                    | TvCommand::Louder
                    | TvCommand::Quieter
            )
            | Intent::Tv(TvCommand::Show(_))
            | Intent::House(HouseCommand::FindPhone(_) | HouseCommand::Climate(house::Indoors::Room(_), _))
            | Intent::Routine(_)
            | Intent::Party(_)
            | Intent::Time
            | Intent::Thanks
            | Intent::Joke
            // A length is the end of the request: "set a timer for 5 minutes", "stop the timer".
            | Intent::Timer(
                TimerCommand::Start { seconds: Some(_), .. }
                    | TimerCommand::Cancel(_)
                    | TimerCommand::Pause(_)
                    | TimerCommand::Resume(_)
                    | TimerCommand::Remaining(_)
            )
    )
}

/// "In here", "this room", "her inne", "dette rommet".
fn mentions_this_room(request: &str) -> bool {
    let text = format!(" {} ", request.to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != ' ', " "));
    [" here ", " this room ", " her inne ", " her ", " dette rommet ", " rommet her "].iter().any(|p| text.contains(p))
}

/// Whether `heard` repeats what was `said`: all its words but at most one were in it ("Left on the
/// one minute timer, sir." after "37 seconds left on the 1 minute timer, sir."). A follow-up adds
/// words of its own ("stop the timer"). After a question the answer may well repeat its words
/// ("Which one?" "The 1 minute timer."), so nothing is an echo then.
fn is_echo(said: &[&str], heard: &str) -> bool {
    if said.last().is_none_or(|last| last.trim_end().ends_with('?')) {
        return false;
    }
    let said: HashSet<String> = said.iter().flat_map(|text| echo_words(text)).collect();
    let heard = echo_words(heard);
    let new = heard.iter().filter(|w| !said.contains(*w)).count();
    // A long stretch of a long answer comes back garbled ("Barnett" as "barn it"), but mostly in its
    // words; nobody asks a follow-up of that many words made of the answer's.
    let garbled = heard.len() >= ECHO_LONG && new * 4 <= heard.len();
    heard.len() >= 2 && (new <= 1 && new * 5 <= heard.len() || garbled)
}

/// Words from which a heard stretch may be a garbled echo.
const ECHO_LONG: usize = 8;

/// Words for comparing what was heard with what was said, with numbers as digits: the speech
/// says "1 minute", the transcriber hears "one minute".
fn echo_words(text: &str) -> Vec<String> {
    const NUMBERS: &[&str] =
        &["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve"];
    // "40-year" is said as "40 year".
    text.split(|c: char| c.is_whitespace() || c == '-')
        .map(voice_assistant::dialog::normalize)
        .filter(|w| !w.is_empty())
        .map(|w| NUMBERS.iter().position(|n| *n == w).map_or(w, |n| n.to_string()))
        .collect()
}

/// "All the lights", "every light", "the whole house": every light, not this room's. Only asked
/// when the request names no room or light ("all the kitchen lights" is the kitchen).
fn mentions_everywhere(request: &str) -> bool {
    request.split_whitespace().map(voice_assistant::dialog::normalize).any(|w| {
        ["all", "every", "everything", "everywhere", "house", "alle", "alt", "overalt", "huset"].contains(&w.as_str())
    })
}

/// The question asked before a change that a misheard request must not make on its own: the
/// camera's privacy mode ("stop the timer" was heard as "Stop the camera.") and arming the alarm.
fn confirm_question(command: &HouseCommand, lang: Lang) -> Option<&'static str> {
    let no = lang == Lang::Norwegian;
    Some(match command {
        HouseCommand::Camera(Switch::Off) if no => "Skal jeg slå på personvernmodus?",
        HouseCommand::Camera(Switch::Off) => "Turn on privacy mode?",
        HouseCommand::Camera(Switch::On) if no => "Skal kameraet ta opp igjen?",
        HouseCommand::Camera(Switch::On) => "Turn privacy mode off?",
        HouseCommand::Alarm(Switch::On) if no => "Skal jeg slå på alarmen?",
        HouseCommand::Alarm(Switch::On) => "Arm the alarm?",
        _ => return None,
    })
}

/// "Yes" or "no" to a question, if the reply is one.
fn confirmation(text: &str) -> Option<bool> {
    const YES: &[&str] = &[
        "yes", "yeah", "yep", "yup", "sure", "ok", "okay", "please", "correct", "do", "it", "ja", "jepp", "gjerne",
        "takk", "sir",
    ];
    const NO: &[&str] =
        &["no", "nope", "nah", "don't", "dont", "not", "cancel", "never", "mind", "nei", "ikke", "la", "være"];
    let words: Vec<String> =
        text.split_whitespace().map(voice_assistant::dialog::normalize).filter(|w| !w.is_empty()).collect();
    let all = |list: &[&str]| !words.is_empty() && words.len() <= 4 && words.iter().all(|w| list.contains(&w.as_str()));
    let first = |list: &[&str]| words.first().is_some_and(|w| list[..4].contains(&w.as_str()));
    if all(NO) || first(NO) {
        Some(false)
    } else if all(YES) {
        Some(true)
    } else {
        None
    }
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
    unreachable_text(error, no).into()
}

/// A device that did not answer is not Home Assistant being out of reach.
fn unreachable_text(error: &anyhow::Error, no: bool) -> &'static str {
    match (voice_assistant::ha::refused(error), no) {
        (true, false) => "Sorry, Home Assistant couldn't do all of that.",
        (true, true) => "Beklager, Home Assistant fikk ikke gjort alt.",
        (false, false) => "Sorry, I couldn't reach Home Assistant.",
        (false, true) => "Beklager, jeg får ikke kontakt med Home Assistant.",
    }
}

pub(crate) fn save_wav(
    dir: &std::path::Path,
    kind: &str,
    audio: &[i16],
    rate: u32,
) -> anyhow::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
    let path = dir.join(format!("{kind}-{stamp}.wav"));
    let spec =
        hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut writer = hound::WavWriter::create(&path, spec)?;
    audio.iter().try_for_each(|&s| writer.write_sample(s))?;
    writer.finalize()?;
    info!(path = %path.display(), "{kind} saved");
    Ok(path)
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

    /// Unrecognised speech after wakes in the living room (unhandled log, 12-17 October 2026).
    #[test]
    fn replies_only_to_what_sounds_like_a_request() {
        for talk in [
            "I'm not a fan of the music video.",
            "Please allow me to sing.",
            "welcome to Tune-In.",
            "it's my one bike playlist.",
            "And clap!",
            "That's for you.",
            "Peace out.",
            "I can't hear you.",
            "I'm a big brother.",
            "Good attire in one minute.",
            "Right.",
            "",
        ] {
            assert!(!sounds_like_a_request(talk), "{talk}");
        }
        for request in [
            "Why are you laughing?",
            "Set a towel for 10 minutes.",
            "Turn on the kitchen.",
            "Hey, um, what's on tonight?",
            "Please dim the hall.",
            "Kan du skru på lyset?",
        ] {
            assert!(sounds_like_a_request(request), "{request}");
        }
    }

    #[test]
    fn asks_before_privacy_mode_and_the_alarm() {
        assert_eq!(confirm_question(&HouseCommand::Camera(Switch::Off), Lang::English), Some("Turn on privacy mode?"));
        assert_eq!(confirm_question(&HouseCommand::Alarm(Switch::On), Lang::English), Some("Arm the alarm?"));
        assert_eq!(confirm_question(&HouseCommand::Camera(Switch::Status), Lang::English), None);
        assert_eq!(confirmation("Yes."), Some(true));
        assert_eq!(confirmation("Yes, please do it."), Some(true));
        assert_eq!(confirmation("Ja takk"), Some(true));
        assert_eq!(confirmation("No."), Some(false));
        assert_eq!(confirmation("No, leave it."), Some(false));
        assert_eq!(confirmation("Nei"), Some(false));
        assert_eq!(confirmation("What time is it?"), None);
        assert_eq!(confirmation("Do you love me?"), None);
    }

    #[test]
    fn tells_its_own_answer_from_a_follow_up() {
        let said = ["37 seconds left on the 1 minute timer, sir."];
        assert!(is_echo(&said, "Left on the one minute timer, sir."));
        assert!(is_echo(&["Timer set for 1 minute, sir."], "One minute sir."));
        assert!(!is_echo(&said, "Stop the timer."));
        assert!(!is_echo(&["It's 20 degrees in the bedroom."], "What about the living room?"));
        assert!(!is_echo(&said, "Timer"));
        assert!(!is_echo(&["You have 2 timers. Which one?"], "The 1 minute timer."));
        assert!(!is_echo(&[], "Turn on the kitchen."));
        let wiki = "According to Wikipedia, Danilo Restivo is an Italian convicted murderer and suspected serial \
                    killer. Restivo is serving a life sentence with a 40-year tariff for murdering his neighbour \
                    Heather Barnett in Bournemouth, England, in November 2002.";
        assert!(is_echo(&[wiki], "With a 40 year tariff for murdering his barn it 40 stop"));
        assert!(!is_echo(&["Tomorrow in Oslo: heavy rain, 3 to 6 degrees."], "And what about tomorrow in Bergen?"));
    }

    #[test]
    fn pauses_for_a_while_or_until_resumed() {
        let pause = Pause::default();
        assert!(!pause.paused());
        pause.pause(Some(60));
        let until = pause.until().unwrap();
        assert!((3590..=3610).contains(&(until - unix_now())));
        pause.pause(None);
        assert_eq!(pause.until(), Some(u64::MAX));
        pause.resume();
        assert!(!pause.paused());
        // A pause that ran out is over without anyone resuming it.
        pause.until.store(unix_now() - 1, Relaxed);
        assert!(!pause.paused());
    }

    #[test]
    fn hushes_music_assistant_not_the_speaker_under_it() {
        let playing = |id: &str, sonos| Playing { id: id.into(), sonos, content: String::new() };
        let music = vec!["media_player.living_room_2".to_owned()];
        let hushed = |players| players_to_hush(players, &music).into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(
            hushed(vec![
                playing("media_player.living_room_2", false),
                playing("media_player.living_room", true),
                playing("media_player.tv", false),
            ]),
            ["media_player.living_room_2", "media_player.tv"]
        );
        // Spotify from a phone plays on the speaker alone: that is paused.
        assert_eq!(hushed(vec![playing("media_player.living_room", true)]), ["media_player.living_room"]);
    }

    #[test]
    fn a_refused_call_is_not_an_unreachable_home_assistant() {
        let refused = anyhow::Error::new(voice_assistant::ha::Refused {
            path: "/api/services/light/turn_off".into(),
            status: 500,
        });
        assert_eq!(unreachable_text(&refused, false), "Sorry, Home Assistant couldn't do all of that.");
        assert_eq!(
            unreachable_text(&anyhow::anyhow!("timeout: global"), false),
            "Sorry, I couldn't reach Home Assistant."
        );
    }

    #[test]
    fn room_talk_in_the_follow_up() {
        assert!(room_talk("With a 40 year tariff for murdering his barn it 40 stop"));
        assert!(!room_talk("And the kitchen too."));
        assert!(!room_talk("Turn on all the lights in the living room and the kitchen."));
        assert!(!room_talk("What is the temperature in the bedroom right now, please?"));
    }

    #[test]
    fn misheard_music_requests() {
        assert_eq!(music_by_sound("Nice music.", true), Some("pause the music".into()));
        assert_eq!(music_by_sound("Please song music.", false), Some("play some music".into()));
        assert_eq!(music_by_sound("Tyson music.", true), Some("pause the music".into()));
        assert_eq!(music_by_sound("Pause the music.", false), None);
        assert_eq!(music_by_sound("I really like this kind of music", true), None);
        assert_eq!(music_by_sound("Nice weather.", true), None);
        assert_eq!(music_by_sound("They're my like songs.", false), Some("play my liked songs".into()));
        assert_eq!(music_by_sound("claim a light song.", false), Some("play my liked songs".into()));
        assert_eq!(music_by_sound("Clean my life's songs.", true), Some("play my liked songs".into()));
        assert_eq!(music_by_sound("I like songs about the sea and the long summer nights", false), None);
        assert_eq!(music_by_sound("Stop.", true), Some("pause the music".into()));
        assert_eq!(music_by_sound("stop it", true), Some("pause the music".into()));
        assert_eq!(music_by_sound("Stop.", false), None);
        assert_eq!(music_by_sound("Put the percentage at 70 percent.", true), Some("set the volume to 70".into()));
        assert_eq!(music_by_sound("Trop with the sign at 50 percent.", true), Some("set the volume to 50".into()));
        assert_eq!(music_by_sound("Put the percentage at 70 percent.", false), None);
    }

    #[test]
    fn quick_commands() {
        let parse = |text: &str| intent::parse(text);
        assert!(heard_by_itself(&parse("pause the music")));
        assert!(heard_by_itself(&parse("next song")));
        assert!(!heard_by_itself(&parse("turn it up")));
        assert!(heard_by_itself(&parse("play some music")));
        assert!(complete_at_once(&parse("pause the music")));
        assert!(complete_at_once(&parse("what time is it")));
        assert!(!complete_at_once(&parse("turn off the lights")));
        assert!(!complete_at_once(&parse("play")));
        assert!(complete_at_once(&parse("turn off the kitchen lights")));
        assert!(complete_at_once(&parse("turn on the kitchen")));
        assert!(!complete_at_once(&parse("turn on the lights in")));
        assert!(complete_at_once(&parse("set the living room to 30 percent")));
        assert!(!complete_at_once(&parse("dim the living room")));
        assert!(complete_at_once(&parse("put the weather on the tv")));
        assert!(complete_at_once(&parse("where is my phone")));
        assert!(complete_at_once(&parse("what's the temperature in the bedroom")));
        assert!(!complete_at_once(&parse("what's the temperature")));
        assert!(complete_at_once(&parse("good night")));
        assert!(complete_at_once(&parse("initiate lockdown")));
        assert!(complete_at_once(&parse("how you doing")));
        assert!(!complete_at_once(&parse("hey")));
    }

    #[test]
    fn data_questions_ask_for_the_data_fetched_ahead() {
        for question in DATA_QUESTIONS {
            let intent = intent::parse(question);
            assert!(matches!(intent, Intent::Weather { place: None, .. } | Intent::Power(_)), "{question}: {intent:?}");
        }
    }

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
