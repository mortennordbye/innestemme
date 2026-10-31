//! The `assistant` processor. The model thread listens and plays the reply; a worker thread
//! handles requests (transcription for Whisper, intent, tool, speech).
//!
//! An energy detector on the model thread cuts the microphone into utterances; the worker
//! transcribes each one (Parakeet, English only, or Whisper, English and Norwegian) and acts on
//! those that start with the name or follow a wake word.
//!
//! Half duplex: while a request is being handled or its reply plays, and for a second after, the
//! microphone is ignored so the assistant does not answer itself through a laptop speaker.

mod follow_up;
mod handle;
mod home;
mod hush;
mod leads;
mod lights;
mod lockdown;
mod pause;
mod preview;
mod processor;
mod quick;
mod render_ahead;
mod screen;
mod shopping;
mod skills;
mod speech;
mod wake;

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tracing::{info, warn};
use voice_assistant::dialog::WakeWord;
use voice_assistant::geo::Location;
use voice_assistant::ha::{HomeAssistant, Target};
use voice_assistant::house::{HouseCommand, Routine};
use voice_assistant::intent::{self, Intent, MusicCommand};
use voice_assistant::jokes::Jokes;
use voice_assistant::lang::Lang;
use voice_assistant::llm::{self, Decision, Llm, Turn};
use voice_assistant::lookup::Lookup;
use voice_assistant::music::Player;
use voice_assistant::news::News;
use voice_assistant::persona::Persona;
use voice_assistant::power::Power;
use voice_assistant::shopping::{Change, ListCommand, ShoppingList};
use voice_assistant::timer::{self, Notice, Timers};
use voice_assistant::transit::Transit;
use voice_assistant::tts::{self, Tts};
use voice_assistant::tv::{self, Tv, TvCommand};
use voice_assistant::vad;
use voice_assistant::voice_gender::{Gender, VoiceGender};
use voice_assistant::weather::Weather;
use voice_assistant::whisper::{Transcribe, Transcript};
use voice_proto::{FRAME_SAMPLES, SAMPLE_RATE};

use crate::activity::{self, Activity, Outcome};
use crate::learned::Learned;
use crate::party::Party;
use crate::render::RenderAhead;
use crate::screen::Screen;
use crate::unhandled::{Reason, UnhandledLog};

use follow_up::room_talk;
use home::{confirm_question, confirmation};
use hush::{Hushed, HUSH_CAP, HUSH_QUIET};
use leads::capitalize;
use lights::LightChange;
use lockdown::Alarm;
use quick::{bare_stop, complete_at_once, heard_by_itself, music_by_sound};
use screen::SCREEN_MOMENT;
use shopping::{LIST_OPEN, LIST_REPLY_WORDS};
use speech::Spoken;
use wake::WakeName;

pub use handle::AssistantHandle;
pub use pause::Pause;
pub use processor::AssistantProcessor;

/// How often the worker checks that the language model's prompt is still the one it warmed.
const WARM_CHECK: Duration = Duration::from_secs(600);
/// Earlier exchanges the language model sees, for follow-ups ("and tomorrow?").
const HISTORY_TURNS: usize = 4;
const HISTORY_MAX_AGE: Duration = Duration::from_secs(300);
/// After a bare name or an answer, speech within this time needs no name.
const FOLLOW_UP: Duration = Duration::from_secs(8);
/// How long "Turn on privacy mode?" waits for its "yes".
const CONFIRM_WINDOW: Duration = Duration::from_secs(30);
/// How often the home's forecast and electricity prices are fetched while idle, so asking for them
/// needs no request and no lead-in.
const DATA_REFRESH: Duration = Duration::from_secs(240);

pub struct AssistantConfig {
    /// Listens for the name in a wake word instead of the main listener: the fast model misses
    /// the name more often, and this check runs while the request is still being spoken.
    pub wake_listener: Option<Box<dyn Transcribe>>,
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

/// What the assistant would say to a phrase typed on the web page.
pub enum Preview {
    /// The answer, styled, and its audio at 24 kHz.
    Spoken { text: String, audio: Vec<i16> },
    /// The request was understood but ends without an answer ("never mind").
    Silent,
    /// Answering would act on the house (lights, music, timers), or needs the language model.
    NotRun(&'static str),
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
    wake_listener: Option<Box<dyn Transcribe>>,
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
    last_lights: Option<(Target, LightChange, Instant)>,
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
    /// A lockdown ended party mode: the party's snapshot holds the lights from before both, for
    /// standing down.
    party_under_lockdown: bool,
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
                    // Checked while the request is still being said: done when a reply needs it,
                    // it held up "didn't catch that" by a quarter of a second on the cluster.
                    self.wake_named = match wake {
                        WakeName::Unknown => None,
                        WakeName::Check(audio) => self.name_in(&audio),
                        WakeName::Heard => Some(true),
                    };
                    // Most wakes without the name are the TV or the room (Loki, 24-31 Oct: about 48
                    // of 58), and pausing paused the TV under them. A real one still acts, over the music.
                    if self.wake_named == Some(false) {
                        info!("no name in the wake word: the music plays on");
                    } else {
                        self.hush();
                    }
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
        let request = self.learned_meaning(self.config.wake.drop_trailing(&request));
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
        let request = self.config.wake.drop_trailing(&request);
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
        let parsed = intent::parse(&request);
        let unknown = parsed == Intent::Unknown;
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
        // A wake word without the name is as unsure as the follow-up window: the TV's "just because
        // 2 bitches ..." set the volume to 70 percent, its "Sorry." was told "Don't mention it.".
        let unnamed = self.woken_at.is_some() && self.wake_named == Some(false);
        let room = !addressed && (self.woken_at.is_none() || unnamed) && room_talk(&request);
        let chat = !addressed && unnamed && matches!(parsed, Intent::SmallTalk(_));
        if !pending && (room || chat || (!addressed && unknown && self.config.llm.is_none())) {
            info!(text = heard.text, "not addressed");
            self.not_understood(&heard.text, audio, ended, heard.lang, start);
            return;
        }
        self.busy.store(true, Relaxed);
        self.request_audio = audio.to_vec();
        self.answer(&request, heard.lang, start);
        self.request_audio.clear();
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
        // "Milk and eggs", "og brunost", "add bacon too": more items while the list is open;
        // "actually remove the milk" takes one off, "no, that's it" closes it.
        if self.list_open.take().is_some_and(|at| at.elapsed() < LIST_OPEN) {
            if voice_assistant::shopping::closes_list(text) {
                return Some(
                    if lang == Lang::Norwegian { "Ok, det var alt." } else { "Okay, that's everything." }.into(),
                );
            }
            if intent == Intent::Unknown && text.split_whitespace().count() <= LIST_REPLY_WORDS {
                if let Some(remove @ ListCommand::Remove(_)) = voice_assistant::shopping::open_list_command(text) {
                    let answer = self.shopping_list(remove, lang, true);
                    // Still open: more items may follow the correction.
                    self.list_open = Some(Instant::now());
                    return Some(answer);
                }
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
            Intent::Date => date_today(&jiff::Zoned::now(), lang),
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
                self.player.paused_for_request(self.hushed.is_some());
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

    fn emit(&self, event: AssistantEvent) {
        // No satellite listening is fine.
        let _ = self.events.send(event);
    }
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

/// "Today is Saturday the 31st of October.", "I dag er det lørdag 31. oktober."
fn date_today(now: &jiff::Zoned, lang: Lang) -> String {
    const MONTHS: [&str; 12] = [
        "januar",
        "februar",
        "mars",
        "april",
        "mai",
        "juni",
        "juli",
        "august",
        "september",
        "oktober",
        "november",
        "desember",
    ];
    const DAYS: [&str; 7] = ["mandag", "tirsdag", "onsdag", "torsdag", "fredag", "lørdag", "søndag"];
    let day = now.day();
    match lang {
        Lang::English => {
            let suffix = match (day % 10, day % 100) {
                (_, 11..=13) => "th",
                (1, _) => "st",
                (2, _) => "nd",
                (3, _) => "rd",
                _ => "th",
            };
            format!("Today is {} the {day}{suffix} of {}.", now.strftime("%A"), now.strftime("%B"))
        }
        Lang::Norwegian => {
            let weekday = DAYS[now.weekday().to_monday_zero_offset() as usize];
            format!("I dag er det {weekday} {day}. {}.", MONTHS[now.month() as usize - 1])
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_the_date() {
        let at = |date: &str| date.parse::<jiff::civil::Date>().unwrap().to_zoned(jiff::tz::TimeZone::UTC).unwrap();
        assert_eq!(date_today(&at("2026-10-31"), Lang::English), "Today is Saturday the 31st of October.");
        assert_eq!(date_today(&at("2026-11-02"), Lang::English), "Today is Monday the 2nd of November.");
        assert_eq!(date_today(&at("2026-11-12"), Lang::English), "Today is Thursday the 12th of November.");
        assert_eq!(date_today(&at("2026-10-31"), Lang::Norwegian), "I dag er det lørdag 31. oktober.");
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
}
