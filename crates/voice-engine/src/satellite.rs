//! Bridges an ESPHome voice satellite (Home Assistant Voice PE, ESPHome voice devices, Linux Voice
//! Assistant) to the assistant, in place of Home Assistant's Assist pipeline.
//!
//! The device's microphone (16 kHz) is resampled to 24 kHz and sent to the engine's own UDP port as
//! an ordinary session, so the audio path is the one every client uses. The assistant's events tell
//! the device what is happening (listening, thinking, speaking), and its answer goes back either
//! streamed over the API (devices with a speaker component) or as a wav URL the device's media
//! player fetches from the engine's HTTP server (Voice PE). Like Home Assistant, the URL goes out
//! with the run's start and the device starts playing it with the first audio, while the rest is
//! still being synthesized. An answer that ends in a question keeps the conversation open: the
//! device listens again without its wake word once it has spoken.
//!
//! With an answer player (a Sonos in Home Assistant), the device only listens: the answer is
//! announced on that player instead, as a URL that streams the audio while it is synthesized, and
//! the device gets no speech events.
//!
//! Wake word: with the device's wake word processing set to "in Home Assistant", the device streams
//! continuously. With a wake word model (`ServerWake`) the bridge scores that stream itself and
//! the engine only hears what follows the wake word; without one, the assistant listens for its own
//! name. With an on-device wake word ("Okay Nabu"), the device starts a run itself and the next
//! utterance is the request.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant; // The system's monotonic clock, which tests can pause and advance.
use tracing::{info, warn};
use voice_assistant::resample;
use voice_assistant::timer::NoticeKind;
use voice_assistant::wakeword::{WakeDetector, WakeModels};
use voice_esphome::proto::{feature, Announce, DeviceInfo, Event, TimerEvent, TimerUpdate, REQUEST_USE_WAKE_WORD};
use voice_esphome::{Device, Incoming};
use voice_proto::{Codec, Header, Kind, HEADER_LEN, MAX_DATAGRAM, PACKET_SAMPLES, SAMPLE_RATE};

use serde_json::json;
use voice_assistant::ha::HomeAssistant;

use crate::assistant::{AnnounceOn, AssistantEvent, AssistantHandle};
use crate::metrics::{Clip, SpeechClips};

const DEVICE_RATE: u32 = 16_000;
/// One engine packet per tick keeps the engine's session clocked at real time.
const TICK: Duration = Duration::from_millis(20);
/// Microphone audio waiting for the engine beyond this is dropped (a stalled tick).
const MAX_MIC_BACKLOG: usize = SAMPLE_RATE as usize;
/// After the wake word, give up when no request follows.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Streamed answer audio kept ahead of playback, like Home Assistant (its buffer is small).
const STREAM_LEAD: Duration = Duration::from_millis(400);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const HELLO_WINDOW: Duration = Duration::from_secs(10);
/// Wake word scores from here (or `ServerWake::check_from`, if lower) up to the threshold are logged
/// as misses, to tune the threshold.
const NEAR_MISS: f32 = 0.2;
/// A name check that has not answered by then (the assistant is busy) lets the wake word go.
const CHECK_TIMEOUT: Duration = Duration::from_millis(1500);
/// Audio kept from before the wake word: the assistant checks it for the name when what follows is
/// not understood, and a run's recording holds the wake word itself.
const WAKE_PREROLL: usize = 2 * DEVICE_RATE as usize;
/// From announcing an answer on the player to its first sound (fetch and start), a little over
/// what the Sonos takes (0.41 s, "answer heard on the device"), so the follow-up does not hear the
/// end of the answer; an echo that still gets through is dropped by the assistant.
const PLAYER_START: Duration = Duration::from_millis(800);
/// The device takes a few tens of milliseconds from the end of speech to waiting for the answer.
const RUN_END_DELAY: Duration = Duration::from_millis(250);
/// Announced over an answer to stop it: 0.3 s at 24 kHz.
const SILENCE: usize = 7200;
/// How long after announcing an answer to watch for it in the microphone, how much of that sets the
/// room's level (no player starts this fast), and how far above that level the answer is.
const ONSET_WATCH: Duration = Duration::from_secs(5);
const ONSET_BASELINE: Duration = Duration::from_millis(250);
const ONSET_RISE_DB: f32 = 15.0;
/// After an answer has played, listen this long for a follow-up without the wake word.
const FOLLOW_UP: Duration = Duration::from_secs(6);
/// An answer with no word from the assistant for this long has lost its end: a lagged event
/// broadcast drops events with only a warning. The longest quiet stretch of a real answer comes
/// before its first audio (the language model, a skill's Home Assistant calls, a slow lead-in);
/// after that, sentences follow each other within seconds. On a speaker device the quiet starts
/// once the audio streamed to it has played, since it goes out at playback pace, long after it
/// was synthesized.
const ANSWER_SILENCE: Duration = Duration::from_secs(60);

pub struct SatelliteConfig {
    /// The device's ESPHome API address, host:port (port 6053).
    pub address: String,
    /// The device's API encryption key, when it has one.
    pub key: Option<[u8; 32]>,
    /// The engine's UDP address, to open the session on.
    pub engine: SocketAddr,
    /// Base URL the device fetches answers from. Default: this host's address toward the device
    /// and `http_port`.
    pub public_url: Option<String>,
    pub http_port: u16,
    /// URL devices: start playing the answer while it is synthesized, instead of when it is complete.
    pub stream_answers: bool,
    /// Speak answers on this Home Assistant media player instead of the device.
    pub answer_player: Option<Arc<AnswerPlayer>>,
    /// On-device wake words to turn on, by phrase or id ("Hey Jarvis"); empty keeps the device's.
    pub wake_words: Vec<String>,
    /// Detect the wake word in the device's continuous stream here.
    pub server_wake: Option<ServerWake>,
    /// Directory for a wav of each run's microphone audio, as the device sent it.
    pub dump: Option<PathBuf>,
}

/// A wake word model run on the stream of a satellite that streams continuously.
pub struct ServerWake {
    pub models: Arc<WakeModels>,
    /// Score from 0 to 1 that counts as the wake word.
    pub threshold: f32,
    /// Scores from here up to `threshold` wake only when the transcriber hears the name in them.
    pub check_from: Option<f32>,
}

/// A Home Assistant media player that announces the answers, e.g. a Sonos.
pub struct AnswerPlayer {
    pub ha: HomeAssistant,
    pub entity: String,
    /// Announcement volume, 0 to 1; the player's own volume when unset.
    pub volume: Option<f32>,
}

impl AnswerPlayer {
    fn announce(&self, url: &str) -> Result<()> {
        self.announce_on(&self.entity, url)
    }

    /// On another player through the same Home Assistant, at the answers' volume.
    fn announce_on(&self, entity: &str, url: &str) -> Result<()> {
        let mut data = json!({
            "entity_id": entity,
            "media_content_id": url,
            "media_content_type": "music",
            "announce": true,
        });
        if let Some(volume) = self.volume {
            // Passed through to the Sonos audio clip API, which takes 0 to 100: 0.65 there is silent.
            data["extra"] = json!({ "volume": (volume * 100.0).round() as u32 });
        }
        self.ha.service("media_player", "play_media", data)
    }
}

/// What the bridge sends to the device. A trait only so that tests can record it.
trait SatelliteDevice {
    fn info(&self) -> &DeviceInfo;
    async fn accept_run(&self) -> Result<()>;
    async fn refuse_run(&self) -> Result<()>;
    async fn event(&self, event: Event, data: &[(&str, &str)]) -> Result<()>;
    async fn audio(&self, pcm: &[i16]) -> Result<()>;
    async fn timer(&self, update: &TimerUpdate) -> Result<()>;
    async fn announce(&self, announce: &Announce) -> Result<()>;
}

impl SatelliteDevice for Device {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }
    async fn accept_run(&self) -> Result<()> {
        Device::accept_run(self).await
    }
    async fn refuse_run(&self) -> Result<()> {
        Device::refuse_run(self).await
    }
    async fn event(&self, event: Event, data: &[(&str, &str)]) -> Result<()> {
        Device::event(self, event, data).await
    }
    async fn audio(&self, pcm: &[i16]) -> Result<()> {
        Device::audio(self, pcm).await
    }
    async fn timer(&self, update: &TimerUpdate) -> Result<()> {
        Device::timer(self, update).await
    }
    async fn announce(&self, announce: &Announce) -> Result<()> {
        Device::announce(self, announce).await
    }
}

/// Scores the device's stream for the wake word. A trait only so that tests can set the scores
/// without a model.
trait WakeScorer: Send {
    fn reset(&mut self);
    fn push(&mut self, pcm: &[i16]) -> Result<Option<f32>>;
}

impl WakeScorer for WakeDetector {
    fn reset(&mut self) {
        WakeDetector::reset(self);
    }
    fn push(&mut self, pcm: &[i16]) -> Result<Option<f32>> {
        WakeDetector::push(self, pcm)
    }
}

/// Keeps a satellite connected, reconnecting with backoff.
pub async fn run(cfg: SatelliteConfig, assistant: AssistantHandle, speech: Arc<SpeechClips>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match session(&cfg, &assistant, &speech).await {
            Ok(()) => info!(address = cfg.address, "satellite disconnected"),
            Err(error) => warn!(address = cfg.address, error = format!("{error:#}"), "satellite session failed"),
        }
        assistant.activity.satellite(None);
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// A near miss of the wake word sent to the assistant to hear whether the name is in it.
struct NameCheck {
    answer: tokio::sync::oneshot::Receiver<Option<bool>>,
    score: f32,
    /// The 16 kHz audio that was checked.
    wake: Vec<i16>,
    /// 16 kHz audio since the check started.
    since: Vec<i16>,
    started: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Run {
    /// No run: the device is not streaming.
    Idle,
    /// Streaming for the wake word, which the assistant detects (its name).
    WakeWord,
    /// The wake word was heard; waiting for the request.
    Listening(Instant),
    /// An answer has just played: listening for a follow-up without the wake word, since then.
    FollowUp(Instant),
    /// The device ended the run before an answer (its own end of speech). With an answer player
    /// the request is still answered there, until the listening timeout.
    Stopped(Instant),
    /// A request is being answered.
    Answering,
}

struct Bridge<'a, D: SatelliteDevice = Device> {
    device: D,
    run: Run,
    base_url: String,
    speech: &'a SpeechClips,
    /// 24 kHz microphone audio for the engine.
    mic: VecDeque<i16>,
    up: resample::Stream,
    /// How answers reach the room, fixed for the session.
    delivery: Delivery,
    dump: Option<PathBuf>,
    /// This run's 16 kHz microphone audio, when dumping.
    recording: Vec<i16>,
    /// The wake word scored here, on the device's continuous stream.
    server_wake: Option<WakeListener>,
    /// When the last answer will have played, to listen for a follow-up from then on.
    follow_up_at: Option<Instant>,
    /// When to end the device's run while it already streams for the wake word: after an answer
    /// went to the player, or a run was done without words.
    run_end_at: Option<Instant>,
    /// The last event of the answer being given (`Run::Answering` or `player_answer`), for the
    /// `ANSWER_SILENCE` watchdog.
    answer_event_at: Option<Instant>,
    /// A request the device stopped (`Run::Stopped`) before it streamed for the wake word again:
    /// its wake. The assistant may still hear it, and is told at the listening timeout when it
    /// does not.
    unheard_since: Option<Instant>,
}

/// The server-side wake word: the detector and what it has heard of the current stream.
struct WakeListener {
    scorer: Box<dyn WakeScorer>,
    /// Score from 0 to 1 that counts as the wake word.
    threshold: f32,
    /// `ServerWake::check_from`.
    check_from: Option<f32>,
    /// The highest score of the current run of scores at or above `NEAR_MISS`.
    near_miss: f32,
    /// This run of near misses has gone to the name check.
    near_miss_checked: bool,
    /// A near miss being checked for the name: the answer, the wake word's audio, and the audio
    /// heard since (the start of the request).
    checking: Option<NameCheck>,
    /// The last `WAKE_PREROLL` of audio while waiting for the wake word.
    preroll: VecDeque<i16>,
}

impl WakeListener {
    fn new(scorer: Box<dyn WakeScorer>, threshold: f32, check_from: Option<f32>) -> Self {
        Self {
            scorer,
            threshold,
            check_from,
            near_miss: 0.0,
            near_miss_checked: false,
            checking: None,
            preroll: VecDeque::new(),
        }
    }

    /// A new stream: nothing from before it (the end of the last wake word or answer, a near miss
    /// cut off by a stop) can wake it.
    fn reset(&mut self) {
        self.scorer.reset();
        self.preroll.clear();
        self.checking = None;
        (self.near_miss, self.near_miss_checked) = (0.0, false);
    }

    /// Scores a chunk of the stream, until the wake word is heard.
    fn listen(&mut self, pcm: &[i16], assistant: &AssistantHandle, answer_playing: bool) -> Result<Option<Woken>> {
        self.preroll.extend(pcm);
        let excess = self.preroll.len().saturating_sub(WAKE_PREROLL);
        self.preroll.drain(..excess);
        if assistant.pause.paused() {
            return Ok(None);
        }
        let threshold = self.threshold;
        let scored = self.scorer.push(pcm)?;
        if let Some(check) = &mut self.checking {
            check.since.extend_from_slice(pcm);
            match check.answer.try_recv() {
                Ok(Some(true)) => {
                    let check = self.checking.take().expect("checked above");
                    info!(score = format!("{:.2}", check.score), "wake word confirmed by its name");
                    return Ok(Some(Woken { wake: check.wake, since: Some(check.since) }));
                }
                Ok(_) => {
                    // Taken outside the log call: its fields are not evaluated when INFO is off.
                    let score = self.checking.take().map_or(0.0, |c| c.score);
                    info!(score = format!("{score:.2}"), "no name in the wake word");
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) if check.started.elapsed() < CHECK_TIMEOUT => {}
                Err(_) => {
                    warn!("the wake word name check did not answer");
                    self.checking = None;
                }
            }
        }
        let Some(score) = scored else { return Ok(None) };
        if score < threshold {
            // Past its peak, the name, if it was said, is in the preroll.
            let check = self.check_from.is_some_and(|from| self.near_miss >= from && score < self.near_miss);
            if check && !self.near_miss_checked && self.checking.is_none() && !answer_playing {
                self.near_miss_checked = true;
                let wake: Vec<i16> = self.preroll.iter().copied().collect();
                let answer = assistant.check_name(resample::to_24k(&wake, DEVICE_RATE));
                let score = self.near_miss;
                self.checking = Some(NameCheck { answer, score, wake, since: Vec::new(), started: Instant::now() });
            }
            if score >= NEAR_MISS.min(self.check_from.unwrap_or(NEAR_MISS)) {
                self.near_miss = self.near_miss.max(score);
            } else if self.near_miss > 0.0 {
                info!(score = format!("{:.2}", self.near_miss), threshold, "wake word missed");
                (self.near_miss, self.near_miss_checked) = (0.0, false);
            }
            return Ok(None);
        }
        info!(score = format!("{score:.2}"), threshold, "wake word");
        self.checking = None;
        Ok(Some(Woken { wake: self.preroll.drain(..).collect(), since: None }))
    }
}

/// The wake word heard: its 16 kHz audio, and the audio after it that was held back while its name
/// was checked (and so the name was heard).
struct Woken {
    wake: Vec<i16>,
    since: Option<Vec<i16>>,
}

/// How a session's answers reach the room, chosen once from the device and the settings. Each
/// way holds only what it needs.
enum Delivery {
    /// Announced on a Home Assistant media player: the device only listens, and gets no speech
    /// events.
    Player {
        player: Arc<AnswerPlayer>,
        answer: Option<PlayerAnswer>,
        /// An answer just announced: when, and the room's quietest level since (dBFS), to log when
        /// the microphone first hears it.
        onset: Option<(Instant, f32)>,
    },
    /// Streamed over the API to a device with a speaker component, at playback pace.
    Speaker(SpeakerStream),
    /// A URL sent with the run's start, which the device plays from the answer's first audio on
    /// (`stream_answers`).
    UrlStreamed(Option<RunClip>),
    /// A wav URL of the whole answer once it is spoken: its 24 kHz audio so far.
    UrlWhole(Vec<i16>),
}

/// The answer on the player.
struct PlayerAnswer {
    /// Its audio so far, streamed to the player. `None` once that stream was ended early (the
    /// device's run ended): the answer still plays what it has.
    clip: Option<Arc<Clip>>,
    announced_at: Instant,
    /// Its length so far, 24 kHz samples.
    samples: usize,
}

/// A speaker device's answer: 16 kHz audio not yet sent, and pacing.
struct SpeakerStream {
    down: resample::Stream,
    outgoing: VecDeque<i16>,
    /// When its first audio came: playback is paced from then.
    start: Option<Instant>,
    streamed: usize,
    /// The answer is complete: the stream ends once `outgoing` has gone out.
    spoken: bool,
}

/// A URL device's run: the answer's URL, sent with the run's start, and its audio so far.
struct RunClip {
    url: String,
    clip: Arc<Clip>,
    /// The device was told to start playing it.
    playing: bool,
}

/// What the end of an answer leaves to the bridge.
enum AfterSpoken {
    /// A speaker device's stream ends once its audio has gone out (`tick`).
    Streaming,
    /// The device's run ends now.
    EndRun,
    /// The answer on the player, when there was one: when it was announced, and its length (24 kHz
    /// samples).
    Player(Option<(Instant, usize)>),
}

impl SpeakerStream {
    fn new() -> Self {
        Self {
            down: resample::Stream::new(SAMPLE_RATE, DEVICE_RATE),
            outgoing: VecDeque::new(),
            start: None,
            streamed: 0,
            spoken: false,
        }
    }

    /// Sends the audio due by now. Returns whether the stream has ended.
    async fn pace(&mut self, device: &impl SatelliteDevice) -> Result<bool> {
        let Some(start) = self.start else { return Ok(false) };
        let allowed = ((start.elapsed() + STREAM_LEAD).as_secs_f64() * DEVICE_RATE as f64) as usize;
        let n = allowed.saturating_sub(self.streamed).min(self.outgoing.len());
        if n > 0 {
            let chunk: Vec<i16> = self.outgoing.drain(..n).collect();
            device.audio(&chunk).await?;
            self.streamed += n;
        }
        if self.spoken && self.outgoing.is_empty() {
            device.event(Event::TtsStreamEnd, &[]).await?;
            self.start = None;
            return Ok(true);
        }
        Ok(false)
    }
}

impl Delivery {
    fn new(speaker: bool, cfg: &SatelliteConfig) -> Self {
        match &cfg.answer_player {
            Some(player) => Delivery::Player { player: player.clone(), answer: None, onset: None },
            None if speaker => Delivery::Speaker(SpeakerStream::new()),
            None if cfg.stream_answers => Delivery::UrlStreamed(None),
            None => Delivery::UrlWhole(Vec::new()),
        }
    }

    fn player(&self) -> Option<&Arc<AnswerPlayer>> {
        match self {
            Delivery::Player { player, .. } => Some(player),
            _ => None,
        }
    }

    /// A run starts. Returns the URL to send with it, for a device that plays the answer from it.
    fn on_start(&mut self, player_answer: bool, speech: &SpeechClips, base_url: &str) -> Option<String> {
        match self {
            // An answer still going to the player keeps its stream.
            Delivery::Player { .. } if player_answer => None,
            Delivery::UrlStreamed(run) => {
                if let Some(old) = run.take() {
                    old.clip.finish();
                }
                let (path, clip) = speech.open(SAMPLE_RATE);
                let url = format!("{base_url}{path}");
                *run = Some(RunClip { url: url.clone(), clip, playing: false });
                Some(url)
            }
            _ => {
                self.finish_clip();
                None
            }
        }
    }

    /// The answer's text, before its audio.
    async fn on_answer(
        &mut self,
        text: &str,
        device: &impl SatelliteDevice,
        speech: &SpeechClips,
        base_url: &str,
    ) -> Result<()> {
        if let Delivery::Player { player, answer, onset } = self {
            // Announced before the first audio exists: the call and the player's fetch take longer
            // than synthesizing the first words.
            if let Some(clip) = answer.take().and_then(|old| old.clip) {
                clip.finish();
            }
            let (path, clip) = speech.open(SAMPLE_RATE);
            let url = format!("{base_url}{path}");
            info!(url, player = player.entity, "answer for the player");
            let player = player.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(error) = player.announce(&url) {
                    warn!(error = format!("{error:#}"), "answer player failed");
                }
            });
            *answer = Some(PlayerAnswer { clip: Some(clip), announced_at: Instant::now(), samples: 0 });
            *onset = Some((Instant::now(), 0.0));
            return Ok(());
        }
        device.event(Event::TtsStart, &[("text", text)]).await?;
        match self {
            Delivery::Speaker(stream) => {
                *stream = SpeakerStream::new();
                device.event(Event::TtsStreamStart, &[]).await?;
            }
            Delivery::UrlStreamed(Some(run)) => run.playing = false,
            Delivery::UrlWhole(answer) => answer.clear(),
            Delivery::UrlStreamed(None) | Delivery::Player { .. } => {}
        }
        Ok(())
    }

    /// The answer's audio, 24 kHz.
    async fn on_speech(&mut self, pcm: &[i16], device: &impl SatelliteDevice) -> Result<()> {
        match self {
            Delivery::Player { answer, .. } => {
                if let Some(answer) = answer {
                    if let Some(clip) = &answer.clip {
                        clip.push(pcm);
                    }
                    answer.samples += pcm.len();
                }
            }
            Delivery::Speaker(stream) => {
                let mut down = Vec::with_capacity(pcm.len() * 2 / 3 + 2);
                stream.down.push(pcm, &mut down);
                stream.outgoing.extend(down);
                stream.start.get_or_insert_with(Instant::now);
            }
            // Every run answered has its clip: it comes with the run's start, and only the run's
            // end or a done without words drops it.
            Delivery::UrlStreamed(run) => {
                if let Some(run) = run {
                    run.clip.push(pcm);
                    if !run.playing {
                        run.playing = true;
                        device.event(Event::IntentProgress, &[("tts_start_streaming", "1")]).await?;
                    }
                }
            }
            Delivery::UrlWhole(answer) => answer.extend_from_slice(pcm),
        }
        Ok(())
    }

    /// The answer is complete.
    async fn on_spoken(
        &mut self,
        device: &impl SatelliteDevice,
        speech: &SpeechClips,
        base_url: &str,
    ) -> Result<AfterSpoken> {
        let (url, streamed) = match self {
            Delivery::Player { answer, .. } => {
                let answer = answer.take();
                if let Some(clip) = answer.as_ref().and_then(|answer| answer.clip.as_ref()) {
                    clip.finish();
                }
                return Ok(AfterSpoken::Player(answer.map(|answer| (answer.announced_at, answer.samples))));
            }
            Delivery::Speaker(stream) => {
                stream.spoken = true;
                return Ok(AfterSpoken::Streaming);
            }
            Delivery::UrlStreamed(run) => match run.take() {
                // Already playing; the device ignores this URL.
                Some(RunClip { url, clip, playing: true }) => {
                    clip.finish();
                    (url, true)
                }
                // No audio came, so the device was never told to play it.
                other => {
                    if let Some(run) = other {
                        run.clip.finish();
                    }
                    (format!("{base_url}{}", speech.put(&[], SAMPLE_RATE)), false)
                }
            },
            Delivery::UrlWhole(answer) => (format!("{base_url}{}", speech.put(answer, SAMPLE_RATE)), false),
        };
        info!(url, streamed, "answer for the device");
        device.event(Event::TtsEnd, &[("url", &url)]).await?;
        Ok(AfterSpoken::EndRun)
    }

    /// Ends a streamed answer, or drops a run's URL nobody will play.
    fn finish_clip(&mut self) {
        let clip = match self {
            Delivery::Player { answer: Some(answer), .. } => answer.clip.take(),
            Delivery::UrlStreamed(run) => run.take().map(|run| run.clip),
            _ => None,
        };
        if let Some(clip) = clip {
            clip.finish();
        }
    }

    /// The wake word was said over the answer on the player: its stream ends, and a moment of
    /// silence announced over it stops it (stopping the player does not stop an announcement).
    fn interrupt(&mut self, speech: &SpeechClips, base_url: &str) {
        let Delivery::Player { player, answer, .. } = self else { return };
        if let Some(clip) = answer.take().and_then(|answer| answer.clip) {
            clip.finish();
        }
        let url = format!("{base_url}{}", speech.put(&[0; SILENCE], SAMPLE_RATE));
        let player = player.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = player.announce(&url) {
                warn!(error = format!("{error:#}"), "could not silence the answer player");
            }
        });
    }

    /// When the audio sent to a speaker device will have played: until then, a quiet assistant is
    /// not a lost answer, since that audio goes out at playback pace.
    fn played_until(&self) -> Option<Instant> {
        let Delivery::Speaker(stream) = self else { return None };
        let samples = stream.streamed + stream.outgoing.len();
        stream.start.map(|start| start + Duration::from_secs_f64(samples as f64 / DEVICE_RATE as f64))
    }

    /// The answer's end was lost: the player's answer no longer counts as playing, and a speaker
    /// device's stream stops.
    fn watchdog(&mut self) {
        match self {
            Delivery::Player { answer, .. } => {
                if let Some(clip) = answer.take().and_then(|answer| answer.clip) {
                    clip.finish();
                }
            }
            Delivery::Speaker(stream) => (stream.spoken, stream.start) = (false, None),
            Delivery::UrlStreamed(_) | Delivery::UrlWhole(_) => {}
        }
    }

    /// Logs how long the player took from the announcement to the answer reaching the room: the
    /// first audio well above the level the room had just after the announcement.
    fn watch_onset(&mut self, pcm: &[i16]) {
        let Delivery::Player { onset, .. } = self else { return };
        let Some((at, quietest)) = onset else { return };
        let since = at.elapsed();
        if since > ONSET_WATCH {
            info!("answer not heard on the device");
            *onset = None;
            return;
        }
        let level = dbfs(pcm);
        if since < ONSET_BASELINE {
            *quietest = if *quietest == 0.0 { level } else { quietest.min(level) };
        } else if level > *quietest + ONSET_RISE_DB {
            info!(after = ?since, "answer heard on the device");
            *onset = None;
        }
    }
}

async fn session(cfg: &SatelliteConfig, assistant: &AssistantHandle, speech: &SpeechClips) -> Result<()> {
    let (device, mut incoming) =
        voice_esphome::Device::connect(&cfg.address, cfg.key.as_ref(), "innestemme", &cfg.wake_words)
            .await
            .with_context(|| format!("connecting to satellite {}", cfg.address))?;
    let base_url = cfg
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}:{}", device.local_addr.ip(), cfg.http_port))
        .trim_end_matches('/')
        .to_owned();
    info!(
        name = device.info.friendly_name,
        model = device.info.model,
        esphome = device.info.esphome_version,
        flags = device.info.voice_assistant_feature_flags,
        answers = match &cfg.answer_player {
            Some(player) => player.entity.as_str(),
            None if device.info.has(feature::SPEAKER) => "streamed",
            None => base_url.as_str(),
        },
        "satellite connected"
    );
    assistant.activity.satellite(Some(&device.info.friendly_name));
    if let Some(words) = &device.wake_words {
        let available: Vec<&str> = words.available.iter().map(|(_, phrase)| phrase.as_str()).collect();
        info!(active = ?words.active, ?available, max = words.max_active, "satellite wake words");
    }

    let udp = UdpSocket::bind("127.0.0.1:0").await?;
    udp.connect(cfg.engine).await?;
    let id = (std::process::id() as u16) ^ (Instant::now().elapsed().subsec_nanos() as u16) ^ 0x5a7e;
    hello(&udp, id).await?;
    let mut events = assistant.subscribe();
    assistant.satellite(vec![device.info.friendly_name.clone(), device.info.name.clone()]);

    let mut b = Bridge::new(device, cfg, base_url, speech);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut seq, mut packet, mut rx) = (0u32, [0u8; MAX_DATAGRAM], [0u8; MAX_DATAGRAM]);

    let result = loop {
        tokio::select! {
            _ = tick.tick() => {
                // Clock the engine: one packet of microphone audio, or silence between runs.
                let pcm: Vec<i16> = (0..PACKET_SAMPLES).map(|_| b.mic.pop_front().unwrap_or(0)).collect();
                Header { kind: Kind::Audio, codec: Codec::PcmS16, session: id, seq, ts_samples: seq.wrapping_mul(PACKET_SAMPLES as u32) }
                    .write(&mut packet);
                let n = voice_proto::pcm_to_bytes(&pcm, &mut packet[HEADER_LEN..]);
                if let Err(error) = udp.send(&packet[..HEADER_LEN + n]).await {
                    break Err(error.into());
                }
                seq = seq.wrapping_add(1);
                // The engine's own audio output is not used: answers come as events.
                while udp.try_recv(&mut rx).is_ok() {}
                if let Err(error) = b.tick(assistant).await {
                    break Err(error);
                }
            }
            message = incoming.recv() => match message {
                None => break Ok(()),
                Some(message) => {
                    if let Err(error) = b.device_message(message, assistant).await {
                        break Err(error);
                    }
                }
            },
            event = events.recv() => match event {
                Ok(event) => {
                    if let Err(error) = b.assistant_event(event).await {
                        break Err(error);
                    }
                }
                Err(RecvError::Lagged(n)) => warn!(n, "satellite missed assistant events"),
                Err(RecvError::Closed) => bail!("the assistant stopped"),
            },
        }
    };
    b.closed(assistant);
    let mut bye = [0u8; HEADER_LEN];
    Header { kind: Kind::Bye, codec: Codec::PcmS16, session: id, seq, ts_samples: 0 }.write(&mut bye);
    let _ = udp.send(&bye).await;
    result
}

impl<'a, D: SatelliteDevice> Bridge<'a, D> {
    fn new(device: D, cfg: &SatelliteConfig, base_url: String, speech: &'a SpeechClips) -> Self {
        Self {
            delivery: Delivery::new(device.info().has(feature::SPEAKER), cfg),
            device,
            run: Run::Idle,
            base_url,
            speech,
            mic: VecDeque::new(),
            up: resample::Stream::new(DEVICE_RATE, SAMPLE_RATE),
            dump: cfg.dump.clone(),
            recording: Vec::new(),
            server_wake: cfg
                .server_wake
                .as_ref()
                .map(|w| WakeListener::new(Box::new(WakeDetector::new(w.models.clone())), w.threshold, w.check_from)),
            follow_up_at: None,
            run_end_at: None,
            answer_event_at: None,
            unheard_since: None,
        }
    }

    async fn device_message(&mut self, message: Incoming, assistant: &AssistantHandle) -> Result<()> {
        match message {
            Incoming::Start(request) => {
                // Paused from the web page: the device's own wake word starts nothing.
                let woke = request.flags & REQUEST_USE_WAKE_WORD == 0;
                if self.run == Run::Answering || (woke && assistant.pause.paused()) {
                    return self.device.refuse_run().await;
                }
                self.device.accept_run().await?;
                let player_answer = self.player_answer();
                match self.delivery.on_start(player_answer, self.speech, &self.base_url) {
                    Some(url) => self.device.event(Event::RunStart, &[("url", &url)]).await?,
                    None => self.device.event(Event::RunStart, &[]).await?,
                }
                self.mic.clear();
                if !woke {
                    self.device.event(Event::WakeWordStart, &[]).await?;
                    if let Run::Stopped(since) = self.run {
                        self.unheard_since = Some(since);
                    }
                    self.run = Run::WakeWord;
                    if let Some(wake) = &mut self.server_wake {
                        wake.reset();
                    }
                } else {
                    info!(wake_word = request.wake_word_phrase, "device woke");
                    self.device.event(Event::SttStart, &[]).await?;
                    self.listening();
                    assistant.woke();
                }
            }
            Incoming::Stop => {
                info!(run = ?self.run, "device ended the run");
                match self.run {
                    Run::Listening(since) if self.delivery.player().is_some() => {
                        // The microphone audio still queued holds the end of the request.
                        self.save_recording();
                        self.device.event(Event::RunEnd, &[]).await?;
                        self.run = Run::Stopped(since);
                    }
                    Run::Answering => {}
                    Run::Listening(_) => self.give_up(assistant).await?,
                    _ => self.end_run().await?,
                }
            }
            Incoming::Audio(pcm) => {
                self.delivery.watch_onset(&pcm);
                assistant.party.feed(&pcm);
                if self.run == Run::WakeWord && self.server_wake.is_some() {
                    return self.listen_for_wake_word(&pcm, assistant).await;
                }
                if self.dump.is_some() && self.run != Run::Idle {
                    self.recording.extend_from_slice(&pcm);
                }
                if matches!(self.run, Run::WakeWord | Run::Listening(_) | Run::FollowUp(_)) {
                    let mut up = Vec::with_capacity(pcm.len() * 3 / 2 + 2);
                    self.up.push(&pcm, &mut up);
                    self.mic.extend(up);
                    let excess = self.mic.len().saturating_sub(MAX_MIC_BACKLOG);
                    self.mic.drain(..excess);
                }
            }
            Incoming::AnnounceFinished => {}
        }
        Ok(())
    }

    async fn assistant_event(&mut self, event: AssistantEvent) -> Result<()> {
        match event {
            AssistantEvent::Listening => {
                if self.run == Run::WakeWord {
                    self.device.event(Event::WakeWordEnd, &[]).await?;
                    self.device.event(Event::SttStart, &[]).await?;
                    self.listening();
                }
            }
            AssistantEvent::Heard { text } => {
                self.unheard_since = None;
                if let Run::Stopped(_) = self.run {
                    self.run = Run::Answering;
                    self.answer_event_at = Some(Instant::now());
                    return Ok(());
                }
                if !matches!(self.run, Run::WakeWord | Run::Listening(_) | Run::FollowUp(_)) {
                    return Ok(());
                }
                // Name and request in one breath: the wake word stage ends here.
                if self.run == Run::WakeWord {
                    self.device.event(Event::WakeWordEnd, &[]).await?;
                    self.device.event(Event::SttStart, &[]).await?;
                }
                self.device.event(Event::SttVadEnd, &[]).await?;
                self.device.event(Event::SttEnd, &[("text", &text)]).await?;
                self.device.event(Event::IntentStart, &[]).await?;
                self.run = Run::Answering;
                // The quiet the watchdog counts starts with this answer, not with the last one's
                // speech: kept from then, a request more than a minute later lost its answer.
                self.answer_event_at = Some(Instant::now());
            }
            AssistantEvent::Answer { text } => {
                if self.run != Run::Answering {
                    return Ok(());
                }
                self.answer_event_at = Some(Instant::now());
                let on_player = self.delivery.player().is_some();
                // "Which room?", "For how long?": the device listens again once it has spoken. Not with
                // an answer player: the device would listen at once, to the player's question.
                let question = text.trim_end().ends_with('?') && !on_player;
                self.device
                    .event(Event::IntentEnd, &[("continue_conversation", if question { "1" } else { "0" })])
                    .await?;
                self.delivery.on_answer(&text, &self.device, self.speech, &self.base_url).await?;
                if on_player && self.server_wake.is_some() {
                    // The device goes back to streaming for the wake word while the player speaks,
                    // so "Hey Jarvis" over the answer stops it. Its run ends a moment later: a
                    // run end while it is still stopping the microphone is ignored, and it waits
                    // for an answer that never comes.
                    self.save_recording();
                    self.mic.clear();
                    self.run_end_at = Some(Instant::now() + RUN_END_DELAY);
                }
            }
            AssistantEvent::Speech(pcm) => {
                if self.run != Run::Answering && !self.player_answer() {
                    return Ok(());
                }
                self.answer_event_at = Some(Instant::now());
                self.delivery.on_speech(&pcm, &self.device).await?;
            }
            AssistantEvent::Spoken => {
                if self.run != Run::Answering && !self.player_answer() {
                    return Ok(());
                }
                let player_answer = self.player_answer();
                match self.delivery.on_spoken(&self.device, self.speech, &self.base_url).await? {
                    AfterSpoken::Streaming => {}
                    AfterSpoken::EndRun => self.end_run().await?,
                    AfterSpoken::Player(answer) => {
                        // With the stream scored here, the device can listen for a follow-up once the
                        // player has finished; the stream is not heard before then, so not the answer.
                        if let (Some((at, samples)), Some(_)) = (answer, &self.server_wake) {
                            let length = Duration::from_secs_f64(samples as f64 / SAMPLE_RATE as f64);
                            self.follow_up_at = Some(at + PLAYER_START + length);
                        }
                        // Else the device's run already ended when the answer started.
                        if !player_answer {
                            // Ends the device's run. After a stop from the device (`Run::Stopped`)
                            // it has ended already; the second run end is harmless, since every
                            // start is refused while answering, so it reaches a device with no run.
                            self.end_run().await?;
                        }
                    }
                }
            }
            AssistantEvent::Done => match self.run {
                Run::Idle => {}
                // The device already ended its run.
                Run::Stopped(_) => self.run = Run::Idle,
                // Done without words ("pause the music"): as after a spoken answer, the run ends a
                // moment later, since a run end while the device stops its microphone is ignored.
                Run::Answering if self.server_wake.is_some() => {
                    self.device.event(Event::IntentEnd, &[("continue_conversation", "0")]).await?;
                    self.delivery.finish_clip();
                    self.save_recording();
                    self.mic.clear();
                    self.run_end_at = Some(Instant::now() + RUN_END_DELAY);
                }
                _ => self.end_run().await?,
            },
            AssistantEvent::Timer(notice) => {
                // Reminders end in speech (an announcement), not in the device's alarm.
                if !self.device.info().has(feature::TIMERS) || notice.reminder {
                    return Ok(());
                }
                let event = match notice.kind {
                    NoticeKind::Started => TimerEvent::Started,
                    NoticeKind::Updated => TimerEvent::Updated,
                    NoticeKind::Cancelled => TimerEvent::Cancelled,
                    NoticeKind::Finished => TimerEvent::Finished,
                };
                self.device
                    .timer(&TimerUpdate {
                        event: event as u32,
                        timer_id: notice.id,
                        name: notice.name,
                        total_seconds: notice.total_seconds,
                        seconds_left: notice.seconds_left,
                        is_active: notice.active,
                    })
                    .await?;
            }
            // What Home Assistant announces plays where answers play, or on a room's speaker.
            AssistantEvent::Announcement { text, audio, on, .. }
                if on != AnnounceOn::Device && self.delivery.player().is_some() =>
            {
                let Some(player) = self.delivery.player().cloned() else { return Ok(()) };
                let url = format!("{}{}", self.base_url, self.speech.put(&audio, SAMPLE_RATE));
                let entity = match on {
                    AnnounceOn::Player(entity) => entity,
                    _ => player.entity.clone(),
                };
                info!(url, text, player = entity, "announcement for the player");
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = player.announce_on(&entity, &url) {
                        warn!(error = format!("{error:#}"), "announcement on the player failed");
                    }
                });
            }
            AssistantEvent::Announcement { text, audio, timer_ring, .. } => {
                // A device that shows timers rings for them itself.
                if timer_ring && self.device.info().has(feature::TIMERS) || !self.device.info().has(feature::ANNOUNCE) {
                    return Ok(());
                }
                let url = format!("{}{}", self.base_url, self.speech.put(&audio, SAMPLE_RATE));
                info!(url, text, "announcement for the device");
                self.device.announce(&Announce { media_id: url, text, ..Announce::default() }).await?;
            }
            // For the TV screen only.
            AssistantEvent::Show(_) => {}
        }
        Ok(())
    }

    /// An answer is going to the player while the device already listens for the wake word again,
    /// so it can be interrupted.
    fn player_answer(&self) -> bool {
        self.server_wake.is_some() && matches!(self.delivery, Delivery::Player { answer: Some(_), .. })
    }

    /// Scores the continuous stream; only audio after the wake word goes on to the engine.
    async fn listen_for_wake_word(&mut self, pcm: &[i16], assistant: &AssistantHandle) -> Result<()> {
        let playing = self.answer_playing();
        let Some(listener) = &mut self.server_wake else { return Ok(()) };
        match listener.listen(pcm, assistant, playing)? {
            Some(Woken { wake, since }) => self.wake(assistant, wake, since).await,
            None => Ok(()),
        }
    }

    /// An answer is on the player, or has just played and a follow-up may come.
    fn answer_playing(&self) -> bool {
        self.player_answer() || self.follow_up_at.is_some_and(|until| Instant::now() < until)
    }

    /// The device listens for a request after a wake word, which ends with it or at the listening
    /// timeout, and replaces an unheard one from before.
    fn listening(&mut self) {
        self.run = Run::Listening(Instant::now());
        self.unheard_since = None;
    }

    /// Starts listening for the request after the wake word in `wake`. `since`: audio after the
    /// wake word that was held back while its name was checked (and so the name was heard).
    async fn wake(&mut self, assistant: &AssistantHandle, wake: Vec<i16>, since: Option<Vec<i16>>) -> Result<()> {
        if let Some(listener) = &mut self.server_wake {
            (listener.near_miss, listener.near_miss_checked) = (0.0, false);
            listener.preroll.clear();
        }
        let playing = self.follow_up_at.take().is_some_and(|until| Instant::now() < until);
        if self.player_answer() || playing {
            info!("wake word over the answer: interrupting it");
            assistant.interrupt();
            self.delivery.interrupt(self.speech, &self.base_url);
        }
        self.device.event(Event::WakeWordEnd, &[]).await?;
        self.device.event(Event::SttStart, &[]).await?;
        if self.dump.is_some() {
            self.recording.clone_from(&wake);
        }
        self.listening();
        match since {
            Some(since) => {
                assistant.woke_named();
                if self.dump.is_some() {
                    self.recording.extend_from_slice(&since);
                }
                let mut up = Vec::with_capacity(since.len() * 3 / 2 + 2);
                self.up.push(&since, &mut up);
                self.mic.extend(up);
            }
            None => assistant.woke_on(resample::to_24k(&wake, DEVICE_RATE)),
        }
        // After the wake job: the worker resets whom it addresses on a wake, so the voice must come later.
        assistant.voice_of(&wake);
        Ok(())
    }

    /// Paces streamed answer audio and times out a wake word with no request.
    async fn tick(&mut self, assistant: &AssistantHandle) -> Result<()> {
        if self.run_end_at.is_some_and(|at| Instant::now() >= at) {
            self.run_end_at = None;
            if self.run == Run::Answering {
                self.run = Run::Idle;
                self.device.event(Event::RunEnd, &[]).await?;
            }
        }
        // The window opens only if the device streams when the answer has played; a deadline
        // reached in any other state is dropped, or a later stream would open it long after.
        let follow_up = self.follow_up_at.is_some_and(|at| Instant::now() >= at);
        if follow_up {
            self.follow_up_at = None;
        }
        match self.run {
            Run::WakeWord if follow_up => {
                info!("listening for a follow-up");
                // The listening lights, without the wake sound.
                self.device.event(Event::SttStart, &[]).await?;
                let before: Vec<i16> =
                    self.server_wake.as_mut().map(|w| w.preroll.drain(..).collect()).unwrap_or_default();
                if self.dump.is_some() {
                    self.recording = before;
                }
                self.run = Run::FollowUp(Instant::now());
                assistant.listen();
            }
            Run::FollowUp(since) if since.elapsed() > FOLLOW_UP => {
                info!("no follow-up");
                return self.end_run().await;
            }
            Run::Listening(since) | Run::Stopped(since) if since.elapsed() > LISTEN_TIMEOUT => {
                info!("no request after the wake word");
                return self.give_up(assistant).await;
            }
            _ => {}
        }
        if self.unheard_since.is_some_and(|since| since.elapsed() > LISTEN_TIMEOUT) {
            info!("no request after the wake word");
            self.unheard_since = None;
            assistant.gave_up();
        }
        if let Delivery::Speaker(stream) = &mut self.delivery {
            if stream.pace(&self.device).await? {
                self.end_run().await?;
            }
        }
        self.watch_answer().await
    }

    /// Ends an answer whose end (`Spoken` or `Done`) was lost, so that later runs are not refused
    /// and a later wake word does not count as said over it.
    async fn watch_answer(&mut self) -> Result<()> {
        let answering = self.run == Run::Answering || self.player_answer();
        let quiet_since = self.answer_event_at.max(self.delivery.played_until());
        let quiet = quiet_since.is_some_and(|at| Instant::now() > at + ANSWER_SILENCE);
        if !answering || !quiet {
            return Ok(());
        }
        warn!(run = ?self.run, "the answer's end did not come: ending it");
        self.delivery.watchdog();
        (self.run_end_at, self.answer_event_at) = (None, None);
        if self.run == Run::Answering {
            return self.end_run().await;
        }
        // The device's run already ended when the answer went to the player.
        Ok(())
    }

    /// The run ends before the assistant heard a request after its wake word: the assistant is
    /// told, or it goes on waiting for one (music held paused, speech held from rendering ahead).
    fn tell_unanswered(&self, assistant: &AssistantHandle) {
        if matches!(self.run, Run::Listening(_) | Run::Stopped(_)) || self.unheard_since.is_some() {
            assistant.gave_up();
        }
    }

    /// Ends a run that heard no request.
    async fn give_up(&mut self, assistant: &AssistantHandle) -> Result<()> {
        self.tell_unanswered(assistant);
        if let Run::Stopped(_) = self.run {
            // The device already ended its run.
            self.run = Run::Idle;
            self.mic.clear();
            return Ok(());
        }
        self.end_run().await
    }

    /// The session has ended: the device is gone, so only the assistant is told.
    fn closed(&self, assistant: &AssistantHandle) {
        self.tell_unanswered(assistant);
    }

    fn save_recording(&mut self) {
        let audio = std::mem::take(&mut self.recording);
        if let (Some(dir), false) = (&self.dump, audio.is_empty()) {
            if let Err(error) = crate::assistant::save_wav(dir, "run", &audio, DEVICE_RATE) {
                warn!(%error, "could not save the run");
            }
        }
    }

    async fn end_run(&mut self) -> Result<()> {
        self.delivery.finish_clip();
        self.save_recording();
        self.run = Run::Idle;
        self.mic.clear();
        self.device.event(Event::RunEnd, &[]).await
    }
}

/// Opens the session on the engine, retrying until it is bound and has room.
async fn hello(udp: &UdpSocket, id: u16) -> Result<()> {
    let mut buf = [0u8; HEADER_LEN];
    Header { kind: Kind::Hello, codec: Codec::PcmS16, session: id, seq: 0, ts_samples: 0 }.write(&mut buf);
    let mut rx = [0u8; MAX_DATAGRAM];
    let deadline = Instant::now() + HELLO_WINDOW;
    while Instant::now() < deadline {
        let _ = udp.send(&buf).await;
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(250), udp.recv(&mut rx)).await {
            if matches!(Header::parse(&rx[..n]), Ok((h, _)) if h.kind == Kind::Hello && h.session == id) {
                // The engine refuses audio until its model thread has reset for the session.
                tokio::time::sleep(Duration::from_millis(20)).await;
                return Ok(());
            }
        }
    }
    bail!("the engine did not accept a session (another client connected?)")
}

/// The level of a chunk of audio in dB below full scale.
fn dbfs(pcm: &[i16]) -> f32 {
    let power = pcm.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / pcm.len().max(1) as f64;
    (10.0 * (power / (32768.0 * 32768.0)).max(1e-12).log10()) as f32
}

#[cfg(test)]
mod tests;
