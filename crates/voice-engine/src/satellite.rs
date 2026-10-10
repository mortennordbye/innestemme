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
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};
use voice_assistant::resample;
use voice_assistant::timer::NoticeKind;
use voice_assistant::wakeword::{WakeDetector, WakeModels};
use voice_esphome::proto::{feature, Announce, Event, TimerEvent, TimerUpdate, REQUEST_USE_WAKE_WORD};
use voice_esphome::{Device, Incoming};
use voice_proto::{Codec, Header, Kind, HEADER_LEN, MAX_DATAGRAM, PACKET_SAMPLES, SAMPLE_RATE};

use serde_json::json;
use voice_assistant::ha::HomeAssistant;

use crate::assistant::{AssistantEvent, AssistantHandle};
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
/// Wake word scores from here up to the threshold are logged as misses, to tune the threshold.
const NEAR_MISS: f32 = 0.2;
/// Audio kept from before the wake word, so a run's recording holds the wake word itself.
const WAKE_PREROLL: usize = 2 * DEVICE_RATE as usize;
/// From announcing an answer on the player to its first sound (fetch and start), a little over
/// what the Sonos takes, so the follow-up does not hear the end of the answer.
const PLAYER_START: Duration = Duration::from_millis(1200);
/// The device takes a few tens of milliseconds from the end of speech to waiting for the answer.
const RUN_END_DELAY: Duration = Duration::from_millis(250);
/// Announced over an answer to stop it: 0.3 s at 24 kHz.
const SILENCE: usize = 7200;
/// After an answer has played, listen this long for a follow-up without the wake word.
const FOLLOW_UP: Duration = Duration::from_secs(6);

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
        let mut data = json!({
            "entity_id": self.entity,
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

/// Keeps a satellite connected, reconnecting with backoff.
pub async fn run(cfg: SatelliteConfig, assistant: AssistantHandle, speech: Arc<SpeechClips>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match session(&cfg, &assistant, &speech).await {
            Ok(()) => info!(address = cfg.address, "satellite disconnected"),
            Err(error) => warn!(address = cfg.address, error = format!("{error:#}"), "satellite session failed"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
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

struct Bridge<'a> {
    device: Device,
    run: Run,
    speaker: bool,
    base_url: String,
    speech: &'a SpeechClips,
    /// 24 kHz microphone audio for the engine.
    mic: VecDeque<i16>,
    up: resample::Stream,
    /// URL devices without streaming: the answer so far, 24 kHz.
    answer: Vec<i16>,
    stream_answers: bool,
    player: Option<Arc<AnswerPlayer>>,
    /// URL devices with streaming, and the answer player: this run's answer URL and its audio so
    /// far. For a device it is sent at the run's start; `clip_playing` says whether the device was
    /// told to start playing it.
    clip: Option<(String, Arc<Clip>)>,
    clip_playing: bool,
    /// Speaker devices: 16 kHz answer audio not yet sent, and pacing.
    down: resample::Stream,
    outgoing: VecDeque<i16>,
    stream_start: Option<Instant>,
    streamed: usize,
    spoken: bool,
    dump: Option<PathBuf>,
    /// This run's 16 kHz microphone audio, when dumping.
    recording: Vec<i16>,
    /// Server-side wake word: the detector and its threshold.
    wake: Option<(WakeDetector, f32)>,
    /// The highest score of the current run of scores at or above `NEAR_MISS`.
    near_miss: f32,
    /// The last `WAKE_PREROLL` of audio while waiting for the wake word, when dumping.
    preroll: VecDeque<i16>,
    /// The answer on the player: when it was announced, and its length so far (24 kHz samples).
    announced: Option<(Instant, usize)>,
    /// When the last answer will have played, to listen for a follow-up from then on.
    follow_up_at: Option<Instant>,
    /// An answer is going to the player while the device already listens for the wake word again,
    /// so it can be interrupted.
    player_answer: bool,
    /// When to end the device's run for `player_answer`.
    run_end_at: Option<Instant>,
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

    let mut b = Bridge {
        speaker: device.info.has(feature::SPEAKER),
        device,
        run: Run::Idle,
        base_url,
        speech,
        mic: VecDeque::new(),
        up: resample::Stream::new(DEVICE_RATE, SAMPLE_RATE),
        answer: Vec::new(),
        stream_answers: cfg.stream_answers,
        player: cfg.answer_player.clone(),
        clip: None,
        clip_playing: false,
        down: resample::Stream::new(SAMPLE_RATE, DEVICE_RATE),
        outgoing: VecDeque::new(),
        stream_start: None,
        streamed: 0,
        spoken: false,
        dump: cfg.dump.clone(),
        recording: Vec::new(),
        wake: cfg.server_wake.as_ref().map(|w| (WakeDetector::new(w.models.clone()), w.threshold)),
        near_miss: 0.0,
        preroll: VecDeque::new(),
        announced: None,
        follow_up_at: None,
        player_answer: false,
        run_end_at: None,
    };
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
                udp.send(&packet[..HEADER_LEN + n]).await?;
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
    let mut bye = [0u8; HEADER_LEN];
    Header { kind: Kind::Bye, codec: Codec::PcmS16, session: id, seq, ts_samples: 0 }.write(&mut bye);
    let _ = udp.send(&bye).await;
    result
}

impl Bridge<'_> {
    async fn device_message(&mut self, message: Incoming, assistant: &AssistantHandle) -> Result<()> {
        match message {
            Incoming::Start(request) => {
                // Paused from the web page: the device's own wake word starts nothing.
                let woke = request.flags & REQUEST_USE_WAKE_WORD == 0;
                if self.run == Run::Answering || (woke && assistant.pause.paused()) {
                    return self.device.refuse_run().await;
                }
                self.device.accept_run().await?;
                // An answer still going to the player keeps its stream.
                if !self.player_answer {
                    self.finish_clip();
                }
                if self.stream_answers && !self.speaker && self.player.is_none() {
                    let (path, clip) = self.speech.open(SAMPLE_RATE);
                    let url = format!("{}{path}", self.base_url);
                    self.device.event(Event::RunStart, &[("url", &url)]).await?;
                    self.clip = Some((url, clip));
                } else {
                    self.device.event(Event::RunStart, &[]).await?;
                }
                self.mic.clear();
                if !woke {
                    self.device.event(Event::WakeWordStart, &[]).await?;
                    self.run = Run::WakeWord;
                    if let Some((detector, _)) = &mut self.wake {
                        // Fresh buffers: the end of the last wake word or answer cannot wake it again.
                        detector.reset();
                        self.preroll.clear();
                    }
                } else {
                    info!(wake_word = request.wake_word_phrase, "device woke");
                    self.device.event(Event::SttStart, &[]).await?;
                    self.run = Run::Listening(Instant::now());
                    assistant.woke();
                }
            }
            Incoming::Stop => {
                info!(run = ?self.run, "device ended the run");
                match self.run {
                    Run::Listening(since) if self.player.is_some() => {
                        // The microphone audio still queued holds the end of the request.
                        self.save_recording();
                        self.device.event(Event::RunEnd, &[]).await?;
                        self.run = Run::Stopped(since);
                    }
                    Run::Answering => {}
                    _ => self.end_run().await?,
                }
            }
            Incoming::Audio(pcm) => {
                if self.run == Run::WakeWord && self.wake.is_some() {
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
                    self.run = Run::Listening(Instant::now());
                }
            }
            AssistantEvent::Heard { text } => {
                if let Run::Stopped(_) = self.run {
                    self.run = Run::Answering;
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
            }
            AssistantEvent::Answer { text } => {
                if self.run != Run::Answering {
                    return Ok(());
                }
                // "Which room?", "For how long?": the device listens again once it has spoken. Not with
                // an answer player: the device would listen at once, to the player's question.
                let question = text.trim_end().ends_with('?') && self.player.is_none();
                self.device
                    .event(Event::IntentEnd, &[("continue_conversation", if question { "1" } else { "0" })])
                    .await?;
                (self.answer, self.spoken, self.stream_start, self.streamed) = (Vec::new(), false, None, 0);
                if let Some(player) = self.player.clone() {
                    // Announced before the first audio exists: the call and the player's fetch take
                    // longer than synthesizing the first words.
                    self.finish_clip();
                    let (path, clip) = self.speech.open(SAMPLE_RATE);
                    let url = format!("{}{path}", self.base_url);
                    info!(url, player = player.entity, "answer for the player");
                    let announced = url.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(error) = player.announce(&announced) {
                            warn!(error = format!("{error:#}"), "answer player failed");
                        }
                    });
                    self.clip = Some((url, clip));
                    self.announced = Some((Instant::now(), 0));
                    if self.wake.is_some() {
                        // The device goes back to streaming for the wake word while the player speaks,
                        // so "Hey Jarvis" over the answer stops it. Its run ends a moment later: a
                        // run end while it is still stopping the microphone is ignored, and it waits
                        // for an answer that never comes.
                        self.player_answer = true;
                        self.save_recording();
                        self.mic.clear();
                        self.run_end_at = Some(Instant::now() + RUN_END_DELAY);
                    }
                    return Ok(());
                }
                self.device.event(Event::TtsStart, &[("text", &text)]).await?;
                self.clip_playing = false;
                self.outgoing.clear();
                self.down = resample::Stream::new(SAMPLE_RATE, DEVICE_RATE);
                if self.speaker {
                    self.device.event(Event::TtsStreamStart, &[]).await?;
                }
            }
            AssistantEvent::Speech(pcm) => {
                if self.run != Run::Answering && !self.player_answer {
                    return Ok(());
                }
                if self.player.is_some() {
                    if let Some((_, clip)) = &self.clip {
                        clip.push(&pcm);
                    }
                    if let Some((_, samples)) = &mut self.announced {
                        *samples += pcm.len();
                    }
                } else if self.speaker {
                    let mut down = Vec::with_capacity(pcm.len() * 2 / 3 + 2);
                    self.down.push(&pcm, &mut down);
                    self.outgoing.extend(down);
                    self.stream_start.get_or_insert_with(Instant::now);
                } else if let Some((_, clip)) = &self.clip {
                    clip.push(&pcm);
                    if !self.clip_playing {
                        self.clip_playing = true;
                        self.device.event(Event::IntentProgress, &[("tts_start_streaming", "1")]).await?;
                    }
                } else {
                    self.answer.extend_from_slice(&pcm);
                }
            }
            AssistantEvent::Spoken => {
                if self.run != Run::Answering && !self.player_answer {
                    return Ok(());
                }
                if self.player.is_some() {
                    // With the stream scored here, the device can listen for a follow-up once the
                    // player has finished; the stream is not heard before then, so not the answer.
                    if let (Some((at, samples)), Some(_)) = (self.announced.take(), &self.wake) {
                        let length = Duration::from_secs_f64(samples as f64 / SAMPLE_RATE as f64);
                        self.follow_up_at = Some(at + PLAYER_START + length);
                    }
                    if std::mem::take(&mut self.player_answer) {
                        // The device's run already ended when the answer started.
                        self.finish_clip();
                        return Ok(());
                    }
                    // Ends the player's stream.
                    self.end_run().await?;
                } else if self.speaker {
                    // The stream ends once the queued audio has gone out (see `tick`).
                    self.spoken = true;
                } else {
                    let url = match self.clip.take() {
                        // Already playing; the device ignores this URL.
                        Some((url, clip)) if self.clip_playing => {
                            clip.finish();
                            url
                        }
                        other => {
                            if let Some((_, clip)) = other {
                                clip.finish();
                            }
                            format!("{}{}", self.base_url, self.speech.put(&self.answer, SAMPLE_RATE))
                        }
                    };
                    info!(url, streamed = self.clip_playing, "answer for the device");
                    self.device.event(Event::TtsEnd, &[("url", &url)]).await?;
                    self.end_run().await?;
                }
            }
            AssistantEvent::Done => match self.run {
                Run::Idle => {}
                // The device already ended its run.
                Run::Stopped(_) => self.run = Run::Idle,
                _ => self.end_run().await?,
            },
            AssistantEvent::Timer(notice) => {
                // Reminders end in speech (an announcement), not in the device's alarm.
                if !self.device.info.has(feature::TIMERS) || notice.reminder {
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
            AssistantEvent::Announcement { text, audio, timer_ring } => {
                // A device that shows timers rings for them itself.
                if timer_ring && self.device.info.has(feature::TIMERS) || !self.device.info.has(feature::ANNOUNCE) {
                    return Ok(());
                }
                let url = format!("{}{}", self.base_url, self.speech.put(&audio, SAMPLE_RATE));
                info!(url, text, "announcement for the device");
                self.device.announce(&Announce { media_id: url, text, ..Announce::default() }).await?;
            }
        }
        Ok(())
    }

    /// Scores the continuous stream; only audio after the wake word goes on to the engine.
    async fn listen_for_wake_word(&mut self, pcm: &[i16], assistant: &AssistantHandle) -> Result<()> {
        if self.dump.is_some() {
            self.preroll.extend(pcm);
            let excess = self.preroll.len().saturating_sub(WAKE_PREROLL);
            self.preroll.drain(..excess);
        }
        let Some((detector, threshold)) = &mut self.wake else { return Ok(()) };
        if assistant.pause.paused() {
            return Ok(());
        }
        let threshold = *threshold;
        let Some(score) = detector.push(pcm)? else { return Ok(()) };
        if score < threshold {
            if score >= NEAR_MISS {
                self.near_miss = self.near_miss.max(score);
            } else if self.near_miss > 0.0 {
                info!(score = format!("{:.2}", self.near_miss), threshold, "wake word missed");
                self.near_miss = 0.0;
            }
            return Ok(());
        }
        info!(score = format!("{score:.2}"), threshold, "wake word");
        self.near_miss = 0.0;
        let playing = self.follow_up_at.take().is_some_and(|until| Instant::now() < until);
        if self.player_answer || playing {
            self.interrupt(assistant);
        }
        self.device.event(Event::WakeWordEnd, &[]).await?;
        self.device.event(Event::SttStart, &[]).await?;
        self.recording = self.preroll.drain(..).collect();
        self.run = Run::Listening(Instant::now());
        assistant.woke();
        Ok(())
    }

    /// The wake word was said over an answer on the player: stop synthesizing it, end its stream,
    /// and silence the player by announcing a moment of silence over it (stopping the player
    /// does not stop an announcement).
    fn interrupt(&mut self, assistant: &AssistantHandle) {
        info!("wake word over the answer: interrupting it");
        assistant.interrupt();
        self.finish_clip();
        (self.player_answer, self.announced) = (false, None);
        if let Some(player) = self.player.clone() {
            let url = format!("{}{}", self.base_url, self.speech.put(&[0; SILENCE], SAMPLE_RATE));
            tokio::task::spawn_blocking(move || {
                if let Err(error) = player.announce(&url) {
                    warn!(error = format!("{error:#}"), "could not silence the answer player");
                }
            });
        }
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
        match self.run {
            Run::WakeWord if self.follow_up_at.is_some_and(|at| Instant::now() >= at) => {
                self.follow_up_at = None;
                info!("listening for a follow-up");
                // The listening lights, without the wake sound.
                self.device.event(Event::SttStart, &[]).await?;
                self.recording = self.preroll.drain(..).collect();
                self.run = Run::FollowUp(Instant::now());
                assistant.listen();
            }
            Run::FollowUp(since) if since.elapsed() > FOLLOW_UP => {
                info!("no follow-up");
                return self.end_run().await;
            }
            Run::Listening(since) if since.elapsed() > LISTEN_TIMEOUT => {
                info!("no request after the wake word");
                return self.end_run().await;
            }
            Run::Stopped(since) if since.elapsed() > LISTEN_TIMEOUT => {
                info!("no request after the wake word");
                self.run = Run::Idle;
                self.mic.clear();
            }
            _ => {}
        }
        if let Some(start) = self.stream_start {
            let allowed = ((start.elapsed() + STREAM_LEAD).as_secs_f64() * DEVICE_RATE as f64) as usize;
            let n = allowed.saturating_sub(self.streamed).min(self.outgoing.len());
            if n > 0 {
                let chunk: Vec<i16> = self.outgoing.drain(..n).collect();
                self.device.audio(&chunk).await?;
                self.streamed += n;
            }
            if self.spoken && self.outgoing.is_empty() {
                self.device.event(Event::TtsStreamEnd, &[]).await?;
                self.stream_start = None;
                self.end_run().await?;
            }
        }
        Ok(())
    }

    fn save_recording(&mut self) {
        let audio = std::mem::take(&mut self.recording);
        if let (Some(dir), false) = (&self.dump, audio.is_empty()) {
            if let Err(error) = crate::assistant::save_wav(dir, "run", &audio, DEVICE_RATE) {
                warn!(%error, "could not save the run");
            }
        }
    }

    /// Ends a streamed answer, or drops a run's URL nobody will play.
    fn finish_clip(&mut self) {
        if let Some((_, clip)) = self.clip.take() {
            clip.finish();
        }
    }

    async fn end_run(&mut self) -> Result<()> {
        self.finish_clip();
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
