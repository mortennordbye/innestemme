//! The `assistant` processor. The model thread listens and plays the reply; a worker thread
//! handles requests (transcription for Whisper, intent, tool, speech).
//!
//! An energy detector on the model thread cuts the microphone into utterances; the worker
//! transcribes each one (Parakeet, English only, or Whisper, English and Norwegian) and acts on
//! those that start with the name or follow a wake word.
//!
//! Half duplex: while a request is being handled or its reply plays, and for a second after, the
//! microphone is ignored so the assistant does not answer itself through a laptop speaker.

mod conversation;
#[cfg(test)]
mod dialogs;
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
use voice_assistant::house::{self, confirm_question, HouseCommand, Routine};
use voice_assistant::intent::{self, Intent, MusicCommand};
use voice_assistant::jokes::Jokes;
use voice_assistant::lang::Lang;
use voice_assistant::llm::{self, Decision, Llm};
use voice_assistant::lockdown::Alarm;
use voice_assistant::lookup::Lookup;
use voice_assistant::memory::{Memory, MemoryCommand};
use voice_assistant::music::Player;
use voice_assistant::news::News;
use voice_assistant::persona::Persona;
use voice_assistant::power::Power;
use voice_assistant::shopping::{Change, ListCommand, ShoppingList};
use voice_assistant::skill::LightChange;
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

use conversation::{AfterAnswer, Awaiting, Conversation, TurnEnd, Wake};
use follow_up::room_talk;
use home::confirmation;
use hush::{HUSH_CAP, HUSH_QUIET};
use leads::capitalize;
use quick::{bare_stop, music_by_sound};
use screen::SCREEN_MOMENT;
use shopping::LIST_REPLY_WORDS;
use speech::Spoken;
use wake::{NameChecker, WakeName};

pub use handle::AssistantHandle;
#[cfg(test)]
pub(crate) use handle::Calls;
pub use pause::Pause;
pub use processor::AssistantProcessor;

/// How often the worker checks that the language model's prompt is still the one it warmed.
const WARM_CHECK: Duration = Duration::from_secs(600);
/// After a bare name or an answer, speech within this time needs no name.
const FOLLOW_UP: Duration = Duration::from_secs(8);
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
    /// Where timers, reminders and alarms are kept, so they survive a restart; in memory only
    /// without it.
    pub timers_file: Option<std::path::PathBuf>,
    /// Where the notes ("remember that ...") are kept, so they survive a restart; in memory only
    /// without it.
    pub memory_file: Option<std::path::PathBuf>,
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
    /// What "what do you think about open source?" plays: a song search.
    pub open_source_music: String,
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
    /// A request is being handled. `words`: its answer will be spoken, by the tests the turn makes
    /// (`Worker::words_follow`); false when it may end without words.
    Heard { text: String, words: bool },
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
    Announcement { text: String, audio: Arc<[i16]>, timer_ring: bool, on: AnnounceOn },
    /// What the TV screen shows now (`Screen::show`'s message), or the face again.
    Show(Option<serde_json::Value>),
}

/// Where an announcement plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnounceOn {
    /// The satellite itself: timers, reminders, warnings.
    Device,
    /// The answer player, where answers play, else the satellite: what Home Assistant announces.
    AnswerPlayer,
    /// Another Home Assistant media player (a room's), through the answer player's Home Assistant;
    /// the satellite without one.
    Player(String),
}

/// Text Home Assistant asked to have spoken (`POST /api/announce`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub text: String,
    pub lang: Lang,
    /// A room by name, for its speaker.
    pub room: Option<String>,
    /// A media player entity id, which wins over `room`.
    pub player: Option<String>,
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
    /// The wake word's name check is done: its result waits in `Worker::wake_check`.
    NameChecked,
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
    /// A Home Assistant call the answer did not wait for failed: say so, in the request's language.
    Failed(String, Lang),
    /// Home Assistant asked for this to be spoken.
    Announce(Announcement),
    /// A button on the web page: a fixed request, acted on without a spoken answer. The answer's
    /// text comes back.
    Act(String, tokio::sync::oneshot::Sender<String>),
    /// A request from a phone or Home Assistant's Assist: acted on like [`Job::Act`], and the
    /// answer comes back in the persona's style, as it would be spoken.
    Ask(String, tokio::sync::oneshot::Sender<String>),
    /// Speech (24 kHz) from Home Assistant's Wyoming client: what the listener hears in it.
    Transcribe(Vec<i16>, tokio::sync::oneshot::Sender<String>),
    /// Text from Home Assistant's Wyoming client: its speech (24 kHz) in the engine's voice.
    Speech(String, tokio::sync::oneshot::Sender<Vec<i16>>),
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
    /// The name check's own model and thread; without one, the check runs here on `transcriber`.
    name_checker: Option<NameChecker>,
    /// The current wake's name check, still out or not yet taken into its `Wake`.
    wake_check: Option<mpsc::Receiver<Option<bool>>>,
    /// The questions asked back, the wake, the follow-up window and what follows the answer.
    conversation: Conversation,
    /// Sentences spoken lately, to tell the answer coming back through the microphone from a request.
    said: VecDeque<(Instant, String)>,
    /// When the speaker stopped talking, for how long the answer took to start.
    speech_ended: Option<Instant>,
    /// The lights switched last, how, and when, for "turn them back on" and "reverse that".
    last_lights: Option<(Target, LightChange, Instant)>,
    shopping: ShoppingList,
    /// The last change to the shopping list and when, for "undo".
    last_list: Option<(Vec<Change>, Instant)>,
    /// The audio of the request being answered, for a second listener (Norwegian items).
    request_audio: Vec<i16>,
    /// The request being answered, recorded in `activity` once its answer is out.
    turn: Option<activity::Turn>,
    timers: Timers,
    /// Notes: "remember that the spare key is in the kitchen drawer".
    memory: Memory,
    /// The notes in the answer being given, lowercased: their sentences are synthesized however the
    /// persona words them, never taken from or added to the speech cache.
    notes_said: Vec<String>,
    /// A request answered for the web page's preview or rendered ahead: questions waiting for an
    /// answer are neither taken nor asked.
    previewing: bool,
    /// The request being answered was said to the assistant: with its name, or after a wake word
    /// with the name. Only such a request, or one that is not room talk, answers "Remember what?".
    request_addressed: bool,
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
    /// Party mode's lights put back after the bridge lets go, still waiting: a lockdown or a stand
    /// down in that second takes it over.
    party_restore: Option<skills::PendingRestore>,
    /// A lockdown started here and not stood down. Its snapshot scene in Home Assistant is
    /// written behind, so a check of it right after the lockdown can miss it.
    locked_down: bool,
    /// When a scene, party mode or a lockdown last changed the lights: light changes before it
    /// cannot be undone.
    light_undo_gone: Option<Instant>,
    /// The TV a lockdown or the open source party put the screen on, and what the TV showed
    /// before, for their end.
    lockdown_tv: Option<(String, voice_assistant::tv::Before)>,
    /// Party mode was started by "what do you think about open source?": ending it also stops
    /// its song and gives the TV back.
    open_source_party: bool,
    /// Which threat the next lockdown ends on.
    threat_turn: usize,
    /// The lockdown's lights blinking.
    alarm: Option<Alarm>,
    /// When the forecast and prices were last fetched ahead of a question.
    refreshed: Option<Instant>,
}

impl Worker {
    /// A wake, a request or its answer, or the few seconds after it that still count.
    fn talking(&self) -> bool {
        self.conversation.wake.is_some()
            || self.conversation.hushed.is_some()
            || self.busy.load(Relaxed)
            || self.conversation.follow_up()
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
            let wait = match self.conversation.after_answer_at() {
                Some(at) => wait.min(at.saturating_duration_since(Instant::now())),
                None => wait,
            };
            let job = match queued.pop_front().map_or_else(|| jobs.recv_timeout(wait), Ok) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => {
                    if self
                        .conversation
                        .hushed
                        .as_ref()
                        .is_some_and(|h| h.last_heard.elapsed() > HUSH_QUIET || h.at.elapsed() > HUSH_CAP)
                    {
                        self.unhush();
                    }
                    self.ring_due_timers();
                    if self.conversation.after_answer_at().is_some_and(|at| Instant::now() >= at) {
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
            if let (true, Some(hushed)) = (heard, &mut self.conversation.hushed) {
                hushed.last_heard = Instant::now();
            }
            if let (true, Some(render)) = (heard, &self.render) {
                render.hold(true);
            }
            // Jobs that act or speak find the music as the wake's check leaves it, as when the check
            // ran on this thread. Drafts and utterances are transcribed while it runs.
            if matches!(
                job,
                Job::Act(..) | Job::Ask(..) | Job::Alert(_) | Job::Failed(..) | Job::Announce(_) | Job::Ready
            ) {
                self.take_wake_name(true);
            }
            match job {
                Job::NothingAsked => self.end_turn(TurnEnd::Silent),
                Job::Woken(at, wake) => self.woken(at, wake),
                Job::NameChecked => self.take_wake_name(false),
                Job::CheckName(audio, reply) => self.check_name(audio, reply),
                Job::Satellite(names) => {
                    // After a restart the device needs the timers again, and those that ended meanwhile are told.
                    let notices = self.timers.connected(Instant::now());
                    self.notify(notices);
                    self.satellite(&names);
                }
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
                Job::Failed(text, lang) => self.announce(&text, Vec::new(), lang, false),
                Job::Announce(announcement) => self.announce_from_home(announcement),
                Job::Act(text, reply) => {
                    let answer = self.typed(&text);
                    info!(request = text, answer, "done from the web page");
                    let _ = reply.send(answer);
                }
                Job::Transcribe(audio, reply) => {
                    let start = Instant::now();
                    let text = match self.transcriber.transcribe(&audio) {
                        Ok(candidates) => candidates.into_iter().next().map(|c| c.text).unwrap_or_default(),
                        Err(error) => {
                            warn!(%error, "transcription for Home Assistant failed");
                            String::new()
                        }
                    };
                    info!(text, took = ?start.elapsed(), "transcribed for Home Assistant");
                    let _ = reply.send(text);
                }
                Job::Speech(text, reply) => {
                    let start = Instant::now();
                    let pcm = self.synthesize(&text);
                    info!(text, took = ?start.elapsed(), "spoken for Home Assistant");
                    let _ = reply.send(pcm);
                }
                Job::Ask(text, reply) => {
                    let answer = self.typed(&text);
                    let answer = match &mut self.persona {
                        Some(persona) if !answer.is_empty() => persona.style(&answer, jiff::Zoned::now().hour() as u8),
                        _ => answer,
                    };
                    info!(request = text, answer, "asked from a phone or Assist");
                    let _ = reply.send(answer);
                }
                Job::Ready => {
                    info!("ready");
                    self.last_lights = None;
                    self.conversation.restart();
                    self.play(&tts::ready_chime());
                }
            }
        }
    }

    fn utterance(&mut self, audio: &[i16], ended: Instant) {
        // The utterance ended in the pause the draft was taken in: same speech, more silence.
        let closing = (vad::END_FRAMES - vad::DRAFT_FRAMES) as usize * FRAME_SAMPLES;
        // Kept only for the rest of a request answered from its draft.
        let undo = self.conversation.draft_undo.take();
        // Audio that does not start with the answered draft is a new utterance: the draft's own
        // end was lost while the engine was deaf to its answer.
        if let Some((draft, _)) = self.conversation.answered_draft.take().filter(|(draft, _)| audio.starts_with(draft))
        {
            if audio.len() <= draft.len() + closing {
                info!("answered from the draft");
                return;
            }
            self.conversation.draft_undo = undo;
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
            .conversation
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
        self.conversation.draft_undo = None;
    }

    /// A draft that is a whole command with nothing that could follow ("pause the music", "what
    /// time is it") is acted on at once, without waiting out the rest of the closing silence.
    fn act_on_draft(&mut self) {
        // A later draft of an utterance already answered from a draft is the same request again:
        // "Hey Jarvis stop the party" was answered twice, 0.6 s apart (16 Nov). The utterance's end
        // still takes the rest of it.
        let Some((audio, candidates)) = &self.conversation.draft else { return };
        let Some(first) = candidates.first() else { return };
        let said = voice_assistant::dialog::words(&self.config.wake.strip(&first.text).unwrap_or(first.text.clone()));
        if let Some((answered, words)) = &mut self.conversation.answered_draft {
            if audio.starts_with(answered) {
                // The same words: the pause was broken by a sound (the answer, the music), not by more
                // speech. The utterance's end follows this draft now, and it is that request too.
                if *words == said {
                    answered.clone_from(audio);
                }
                return;
            }
        }
        let follow_up = self.conversation.follow_up();
        let request = match self.config.wake.strip(&first.text) {
            Some(rest) if !rest.is_empty() => rest,
            None if self.conversation.wake.is_some() => first.text.clone(),
            None if follow_up && !room_talk(&first.text) => first.text.clone(),
            _ => return,
        };
        let named = self.config.wake.strip(&first.text).is_some();
        let request = self.config.wake.drop_trailing(&request);
        let request = self.learned_meaning(self.leaving_by_sound(&request, named).unwrap_or(request));
        if quick::goes_on(&request) {
            return;
        }
        let intent = intent::parse(&request);
        if intent::split_commands(&request).is_some() || !intent.meta().complete_at_once {
            return;
        }
        let Some((audio, candidates)) = self.conversation.draft.take() else { return };
        let undo = match &intent {
            Intent::Music(command) => self.draft_undo(command),
            _ => None,
        };
        let pause = Duration::from_secs_f32((vad::DRAFT_FRAMES as usize * FRAME_SAMPLES) as f32 / SAMPLE_RATE as f32);
        let now = Instant::now();
        self.speech_ended = now.checked_sub(pause);
        self.conversation.answered_draft = Some((audio.clone(), said));
        self.heard(&candidates, &audio, now, now, true);
        self.conversation.draft_undo = undo;
    }

    /// What was said, as transcribed: a wake word, a request, or nothing for the assistant.
    fn heard(&mut self, candidates: &[Transcript], audio: &[i16], ended: Instant, start: Instant, from_draft: bool) {
        // The reply to the request depends on the name in the wake, and the music is paused first.
        self.take_wake_name(true);
        let follow_up = self.conversation.follow_up();
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
                self.conversation.follow_up_until = Some(Instant::now() + FOLLOW_UP);
                self.emit(AssistantEvent::Listening);
                self.hush();
                self.play(&tts::wake_chime());
                return;
            }
            Some(rest) => (rest, true),
            // While the device's wake run lasts, a request counts however long the pause before it.
            None if follow_up || self.conversation.wake.is_some() => (heard.text.clone(), false),
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
        let playing = self.conversation.hushed.is_some() || (bare_stop(&request) && self.music_playing());
        let request = match music_by_sound(&request, playing) {
            Some(meant) if addressed || self.conversation.wake.is_some() => {
                info!(heard = request, meant, "music request by its shape");
                meant
            }
            _ => request,
        };
        let request = match self.leaving_by_sound(&request, addressed) {
            Some(meant) => {
                info!(heard = request, meant, "leaving by its shape");
                meant
            }
            None => request,
        };
        let request = self.learned_meaning(request);
        // A "Which one?" left unanswered goes, before a follow-on asks what waits for a reply.
        self.timers.forget_stale(Instant::now());
        // Without the name, only act on something recognisable: in the follow-up window the
        // microphone also hears the room.
        // With a language model, it decides whether unrecognised words were meant for the assistant.
        let parsed = intent::parse(&request);
        // The room-talk filter judges the words as heard, not the request they were joined into.
        let as_heard = request.clone();
        let meant = self.follow_on(&request, &parsed).or_else(|| self.party_stop(&request, &parsed));
        // A joined request holds the last topic: no rewrite learned from it holds without that.
        let joined = meant.is_some();
        let (request, parsed) = meant.unwrap_or((request, parsed));
        let unknown = parsed == Intent::Unknown;
        if let (false, false, Some(learned)) = (unknown, joined, &self.config.learned) {
            if let Some(pair) = learned.understood(&request) {
                info!(heard = pair.said, meant = pair.meant, "correction learned");
            }
        }
        let pending = match self.awaiting() {
            // A question waiting for "yes" only takes a yes or a no: other words without the name
            // are the room.
            Some(Awaiting::Confirm | Awaiting::Forget) => confirmation(&request).is_some(),
            // "Which one?" takes only an answer ("the first one"), so other words without the name
            // still face the room-talk filter.
            Some(Awaiting::TimerDuration | Awaiting::WhichTimer) => self.timers.takes_answer(&request),
            Some(Awaiting::LightsRoom | Awaiting::SceneRoom | Awaiting::ListItems) => true,
            // "Remember what?" is answered like any request: its answer still faces the filters.
            Some(Awaiting::RememberWhat) | None => false,
        };
        // The follow-up window hears the room and the end of the answer too: "with a 40 year tariff
        // for murdering his barn it 40 stop" set the volume.
        // A wake word without the name is as unsure as the follow-up window: the TV's "just because
        // 2 bitches ..." set the volume to 70 percent, its "Sorry." was told "Don't mention it.".
        let unnamed = self.conversation.unnamed();
        let room = !addressed && (self.conversation.wake.is_none() || unnamed) && room_talk(&as_heard);
        let chat = !addressed && unnamed && matches!(parsed, Intent::SmallTalk(_));
        if !pending && (room || chat || (!addressed && unknown && self.config.llm.is_none())) {
            info!(text = heard.text, "not addressed");
            self.not_understood(&heard.text, audio, ended, heard.lang, start);
            return;
        }
        self.busy.store(true, Relaxed);
        self.request_addressed = addressed || (self.conversation.wake.is_some() && !unnamed);
        self.request_audio = audio.to_vec();
        self.answer(&request, heard.lang, start);
        self.request_audio.clear();
    }

    /// "Leaving.", "You're leaving." are the leaving routine only when said to the assistant: with
    /// its name, or in a device wake run whose wake word held the name. The routine turns off the
    /// house, and the follow-up window hears the room and the TV too.
    fn leaving_by_sound(&self, request: &str, addressed: bool) -> Option<String> {
        let said_to_us = addressed || (self.conversation.wake.is_some() && !self.conversation.unnamed());
        (said_to_us && house::misheard_leaving(&voice_assistant::dialog::words(request))).then(|| "I'm leaving".into())
    }

    fn transcribe_draft(&mut self, audio: Vec<i16>) {
        let transcriber = &mut self.transcriber;
        self.conversation.draft = match transcriber.transcribe(&audio) {
            Ok(candidates) => Some((audio, candidates)),
            Err(error) => {
                warn!(%error, "draft transcription failed");
                None
            }
        };
    }

    fn answer(&mut self, text: &str, lang: Lang, start: Instant) {
        self.interrupted.store(false, Relaxed);
        // A slow answer (weather, departures) opens with a lead-in rendered ahead of time, which
        // plays while the data is fetched and its sentence synthesized.
        let lead = self.lead_in(text, lang);
        let words = self.words_follow(text, lead.is_some());
        self.emit(AssistantEvent::Heard { text: text.to_owned(), words });
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
            // Nothing asked back waits on once the conversation is over ("never mind").
            self.forget_questions();
            if lead.is_some() {
                self.finish(spoken, start);
            } else {
                self.end_turn(TurnEnd::Ended);
            }
            return;
        };
        let failed = ["Sorry", "Beklager"].iter().any(|s| answer.starts_with(s));
        if !failed && intent::split_commands(text).is_none() && intent::parse(text).meta().heard_by_itself {
            let turn = self.turn_for(text, Some(answer.clone()), Outcome::Done, start);
            info!(request = text, answer, skill = turn.skill, after = ?start.elapsed(), "done without words");
            let after_ms = self.speech_ended.take().map(|ended| ended.elapsed().as_millis() as u64);
            self.config.activity.record(activity::Turn { after_ms, ..turn });
            self.conversation.answered(text);
            self.conversation.remember(text, answer);
            self.end_turn(TurnEnd::Done);
            return;
        }
        let turn = self.turn_for(text, Some(answer.clone()), Outcome::Answered, start);
        info!(request = text, answer, ?lang, skill = turn.skill, after = ?start.elapsed(), "answering");
        self.turn = Some(turn);
        // The rules' own fallback, or a skill that could not do it ("Sorry, I couldn't find ...").
        let fallback = ["Sorry, I can't help with that yet.", "Beklager, det kan jeg ikke hjelpe med ennå."];
        if fallback.iter().any(|f| answer.starts_with(f)) {
            self.unhandled(Reason::Unknown, text, Some(&answer));
        } else if ["Sorry", "Beklager"].iter().any(|s| answer.starts_with(s) || answer.contains(&format!(". {s}"))) {
            self.unhandled(Reason::Apologised, text, Some(&answer));
        }
        self.conversation.answered(text);
        self.conversation.remember(text, answer.clone());
        let answer = match &mut self.persona {
            Some(persona) if lang == Lang::English && (lead.is_some() || plain) => persona.after_lead(&answer),
            Some(persona) if lang == Lang::English => persona.style(&answer, jiff::Zoned::now().hour() as u8),
            _ => answer,
        };
        // A note as the persona says it ("Your bike is in the shed, sir.") is still a note.
        for sentence in answer.split_inclusive(['.', '?', '!']).map(str::trim) {
            let lower = sentence.to_lowercase();
            if self.notes_said.iter().any(|note| lower.contains(note.as_str())) {
                self.live.insert(sentence.to_owned());
            }
        }
        self.notes_said.clear();
        if lead.is_none() {
            self.emit(AssistantEvent::Answer { text: answer.clone() });
        }
        self.speak(&answer, lang, &mut spoken);
        self.finish(spoken, start);
    }

    /// Whether the answer to `text` will be spoken, by the tests the turn makes below: a satellite
    /// announces the answer on a player before it is worded only then, since an announce ducks the
    /// music. A lead-in is words. False where it may end quietly: done without words, "never mind",
    /// a sum that does not add up, and a request only a language model may take (it can ignore it).
    fn words_follow(&self, text: &str, lead: bool) -> bool {
        if lead {
            return true;
        }
        let spoken = |intent: &Intent| match intent {
            Intent::Cancel | Intent::Unknown => false,
            Intent::Calculate(said) => voice_assistant::calc::parse(said).is_some(),
            Intent::Lookup(_) => self.config.llm.is_none(),
            _ => true,
        };
        match intent::split_commands(text) {
            // Several requests end quietly only when each part does.
            Some(parts) => parts.iter().any(|part| spoken(&intent::parse(part))),
            None => {
                let parsed = intent::parse(text);
                !parsed.meta().heard_by_itself && spoken(&parsed)
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

    /// The spoken answer, or `None` to end the conversation quietly.
    fn compose(&mut self, text: &str, lang: Lang) -> Option<String> {
        let intent = match intent::parse(text) {
            // A language model knows facts the opening of an article leaves out; it can still look
            // things up with its tool.
            Intent::Lookup(_) if self.config.llm.is_some() => Intent::Unknown,
            intent => intent,
        };
        // No note about it: as the other skills take it ("what did I tell you about the timer",
        // "forget the milk"), unless none does.
        let intent = match intent {
            Intent::Memory(ref command @ (MemoryCommand::Recall(_) | MemoryCommand::Forget(_)))
                if !self.memory.finds(command) =>
            {
                match intent::parse_without_notes(text) {
                    Intent::Unknown => intent,
                    other => other,
                }
            }
            intent => intent,
        };
        // A question asked by voice waits for a spoken reply: a preview, a refresh ahead of time or
        // a typed request (a web button, a phone) neither takes nor drops it, as it cannot hear the
        // reply.
        let asking = !self.previewing;
        if asking {
            if let Some(answer) = self.reply(text, &intent, lang) {
                return Some(answer);
            }
        }
        // "Where is the spare key": a note about it, unless a person, a zone, a phone or a known place
        // is meant ("where's my phone" still rings it).
        if let Some(note) = self.memory.where_is(text).filter(|_| !self.where_is_elsewhere(text, &intent)) {
            self.keep_live(&note);
            return Some(note);
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
                let history = self.conversation.recent_turns();
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

    /// The answer to the question asked back, when `text` answers it. Whatever was waiting is
    /// dropped either way: the next words were its reply, or something else.
    fn reply(&mut self, text: &str, intent: &Intent, lang: Lang) -> Option<String> {
        let awaiting = self.awaiting();
        // Taken before the answer, which may ask again ("Which room?").
        let lights = self.conversation.pending_lights.take();
        let scene = self.conversation.pending_scene.take();
        let confirm = self.conversation.pending_confirm.take();
        self.conversation.list_open = None;
        let unknown = *intent == Intent::Unknown;
        let answer = match awaiting? {
            // "Set a timer." "For how long?" "Ten minutes."
            Awaiting::TimerDuration | Awaiting::WhichTimer => {
                self.timers.answer_pending(text, lang, Instant::now()).map(|(answer, notices)| {
                    self.notify(notices);
                    answer
                })
            }
            Awaiting::LightsRoom => match lights {
                Some((on, _)) if unknown => {
                    let switch = if on { "on" } else { "off" };
                    self.conversation.answered_as = Some(format!("turn {switch} the lights in {text}"));
                    Some(self.lights(text, on, lang))
                }
                _ => None,
            },
            // "Milk and eggs", "og brunost", "add bacon too": more items while the list is open;
            // "actually remove the milk" takes one off, "no, that's it" closes it.
            Awaiting::ListItems => self.list_reply(text, unknown, lang),
            // "Forget everything I remember?" "Yes."
            Awaiting::Forget => confirmation(text).map(|yes| self.memory.answer_forget(yes, lang)),
            // "Remember what?" "The bins go out on Tuesday."
            Awaiting::RememberWhat => {
                self.memory.clear_question();
                let meant = self.request_addressed || !room_talk(text);
                (meant && matches!(intent, Intent::Unknown | Intent::Lookup(_)))
                    .then(|| self.memory.remember(text, lang))
            }
            Awaiting::Confirm => match (confirmation(text), confirm) {
                (Some(true), Some((command, _))) => Some(self.house(&command, lang)),
                (Some(false), _) => {
                    Some(if lang == Lang::Norwegian { "Ok, jeg lar det være." } else { "Okay, I'll leave it." }.into())
                }
                // Anything else is a new request.
                _ => None,
            },
            Awaiting::SceneRoom => match scene {
                Some((scene, _)) if unknown => Some(self.scene(&format!("{scene} in {text}"), lang)),
                _ => None,
            },
        };
        if answer.is_none() {
            self.timers.clear_pending();
            self.memory.clear_question();
        }
        answer
    }

    /// A reply while the shopping list is open: items, changes, or the list closed.
    fn list_reply(&mut self, text: &str, unknown: bool, lang: Lang) -> Option<String> {
        if voice_assistant::shopping::closes_list(text) {
            return Some(if lang == Lang::Norwegian { "Ok, det var alt." } else { "Okay, that's everything." }.into());
        }
        if !unknown || text.split_whitespace().count() > LIST_REPLY_WORDS {
            return None;
        }
        let changes = voice_assistant::shopping::open_list_commands(text);
        // "Remove milk and add bread", "add bread and put milk on", "check if we have eggs": each
        // change in turn. A single "add X" goes the way of plain items, which may be heard again in
        // Norwegian.
        if changes.len() > 1 || changes.iter().any(|c| !matches!(c, ListCommand::Add(_))) {
            return Some(self.shopping_changes(changes, lang));
        }
        let items = self.items_heard(text);
        (!items.is_empty()).then(|| self.shopping_list(ListCommand::Add(items), lang, true))
    }

    /// A note's sentences are synthesized, never kept by the speech cache: here as they are, and in
    /// `answer` again as the persona words them.
    fn keep_live(&mut self, text: &str) {
        let sentences: Vec<&str> =
            text.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty()).collect();
        self.live.extend(sentences.iter().map(|s| (*s).to_owned()));
        let cores =
            sentences.iter().map(|s| s.trim_end_matches(['.', '?', '!']).to_lowercase()).filter(|s| !s.is_empty());
        self.notes_said.extend(cores);
    }

    /// Whether "where is X" means a person, a zone, a phone or a known place rather than a note.
    fn where_is_elsewhere(&self, text: &str, intent: &Intent) -> bool {
        match intent {
            Intent::House(HouseCommand::FindPhone(_) | HouseCommand::PhoneBattery(_)) => true,
            // "Where is Ingrid's bike" is about the bike.
            Intent::WhereIs { .. } if possessive(text) => false,
            Intent::WhereIs { name, .. } => {
                if voice_assistant::geo::known(name, self.home_location.as_ref()).is_some() {
                    return true;
                }
                let people = self.home_assistant.as_ref().and_then(|ha| ha.people().ok()).unwrap_or_default();
                // A person by the whole name asked: a sound-alike for one word, else the full name.
                let one_word = name.split_whitespace().count() == 1;
                voice_assistant::people::find(&people, name)
                    .is_some_and(|p| one_word || p.name.eq_ignore_ascii_case(name))
                    || people.iter().any(|p| p.state.eq_ignore_ascii_case(name))
            }
            _ => false,
        }
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

    /// A request that arrives as text (a web page button, a phone): acted on without a spoken
    /// answer, and recorded like a spoken one. The answer's text, empty when it ends quietly.
    fn typed(&mut self, text: &str) -> String {
        let start = Instant::now();
        // A question asked by voice ("Forget everything I remember?") is neither answered nor
        // dropped by a request that cannot hear the reply.
        self.previewing = true;
        let answer = self.compose(text, Lang::English).unwrap_or_default();
        self.previewing = false;
        let turn = self.turn_for(text, Some(answer.clone()), Outcome::Done, start);
        self.config.activity.record(turn);
        self.conversation.typed_done();
        answer
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
            Intent::Weather { place, day, focus } => self.weather(place, day, focus, lang),
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
                // A command for a room, and whether that room is this one.
                let (named, inner) = match &command {
                    MusicCommand::InRoom(room, inner) => (Some(room.as_str()), Some(inner.as_ref())),
                    MusicCommand::InRooms(rooms, inner) => (rooms.first().map(String::as_str), Some(inner.as_ref())),
                    _ => (None, None),
                };
                let here = named.is_some_and(|room| self.is_this_room(room));
                // The draft acted here at once, and the request went on to name another room: what
                // it did here is undone.
                if let (Some(undo), Some(room)) = (self.conversation.draft_undo.take(), named) {
                    if !here {
                        info!(room, ?undo, "the request named another room: undoing it here");
                        self.player.run(self.home_assistant.as_ref(), &undo, lang);
                    }
                }
                // What it does to this room's music, if anything.
                let for_here = match (named, inner) {
                    (None, _) => Some(&command),
                    (Some(_), Some(inner)) if here => Some(inner),
                    _ => None,
                };
                // New music replaces what was paused for the request, and "play" plays it itself;
                // pausing, stopping or muting it keeps it from playing on after the answer. Music
                // moved from here is not resumed here. Queued music and another room's leave this
                // room's music to play on after the answer.
                let clears = match for_here {
                    Some(
                        MusicCommand::Pause
                        | MusicCommand::Resume
                        | MusicCommand::Stop
                        | MusicCommand::Liked
                        | MusicCommand::Playlist(_)
                        | MusicCommand::Song(_)
                        | MusicCommand::Artist(_)
                        | MusicCommand::Album(_),
                    ) => true,
                    Some(MusicCommand::Mute(true)) => named.is_some(),
                    Some(MusicCommand::MoveTo(_, from)) => from.as_deref().is_none_or(|from| self.is_this_room(from)),
                    _ => false,
                };
                if clears {
                    self.conversation.hushed = None;
                }
                if command == MusicCommand::Stop || inner == Some(&MusicCommand::Stop) {
                    self.party_off();
                }
                let held: Vec<String> =
                    self.conversation.hushed.iter().flat_map(|h| h.players.iter().map(|p| p.id.clone())).collect();
                self.player.paused_for_request(self.conversation.hushed.is_some(), &held);
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
            Intent::WhereIs { name, zone, arriving } => self.where_is(&name, zone.as_deref(), arriving, lang),
            Intent::Convert(conversion) => voice_assistant::units::answer(&conversion, lang),
            Intent::Memory(command) => {
                let answer = self.memory.run(command, lang);
                let notes: Vec<String> = self.memory.sentences().map(str::to_owned).collect();
                notes.iter().for_each(|note| self.keep_live(note));
                answer
            }
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
            Intent::Cancel => {
                // "Stop" while the alarm rings: it is stopped, quietly.
                if self.timers.alarm_ringing(Instant::now()) {
                    self.timers.run(timer::TimerCommand::StopAlarm, lang, Instant::now());
                }
                return None;
            }
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
                    self.conversation.pending_confirm = Some((command, Instant::now()));
                    question.into()
                }
                None => self.house(&command, lang),
            },
            Intent::Routine(routine) => {
                // Leaving and bedtime pause the music themselves; coming home may start new music.
                if routine != Routine::Home || self.config.home_music.is_some() {
                    self.conversation.hushed = None;
                }
                match routine {
                    Routine::Lockdown => self.lockdown(lang),
                    Routine::StandDown => self.stand_down(lang),
                    Routine::OpenSource => self.open_source(lang),
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

    /// Rings for timers that ran out and speaks what they were for. One that ended while the
    /// engine was down is told instead; an alarm rings even then.
    fn ring_due_timers(&mut self) {
        for (timer, notices) in self.timers.due(Instant::now()) {
            let text = timer::finished_text(&timer);
            info!(text, alarm = timer.is_alarm(), late = timer.late, "timer done");
            self.notify(notices);
            let chime = if timer.is_reminder() { tts::reminder_chime() } else { tts::alarm_chime() };
            let ring = timer.is_alarm() || (!timer.is_reminder() && timer.late.is_none());
            self.announce(&text, chime, timer.lang, ring);
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

/// The answer when no Home Assistant is set up, in the listener's language.
fn not_connected(no: bool) -> String {
    if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }.into()
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

/// "Ingrid's bike": a name owning a thing, so the question is about the thing.
fn possessive(text: &str) -> bool {
    let words: Vec<String> = text.split_whitespace().map(voice_assistant::dialog::normalize).collect();
    words.windows(2).any(|p| p[0].len() > 2 && p[0].ends_with("'s") && !p[1].is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thing_someone_owns_is_the_thing() {
        assert!(possessive("where is Ingrid's bike"));
        assert!(possessive("where's Ola's jacket?"));
        assert!(!possessive("where is Ingrid"));
        assert!(!possessive("where is Ingrid's"));
    }

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
