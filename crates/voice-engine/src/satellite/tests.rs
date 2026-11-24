//! The bridge driven by a fake device, a recording assistant handle and a fake Home Assistant for
//! the answer player, on tokio's paused clock. Each step returns what the bridge did in it: the
//! device's messages in order, then the assistant calls, then the flags it set.
//!
//! The tests describe the bridge as it is. A test marked `#[ignore = "bug: ..."]` describes the
//! behaviour a bug fix should give; it fails until then.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

use voice_esphome::proto::{feature, VoiceAssistantRequest};

use super::*;
use crate::assistant::Calls;

const BASE: &str = "http://engine.test";
const ENTITY: &str = "media_player.answers";
const NOTHING: [&str; 0] = [];
/// One device chunk of microphone audio, 64 ms at 16 kHz.
const CHUNK: usize = 1024;

type Log = Arc<Mutex<Vec<String>>>;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    /// The device plays answers streamed over the API.
    Speaker,
    /// The device fetches the whole answer as a wav URL once it is spoken.
    UrlWhole,
    /// The device fetches the answer's URL while it is synthesized (`stream_answers`).
    UrlStreamed,
    /// The answer is announced on a Home Assistant media player.
    Player,
}

const ALL: [Mode; 4] = [Mode::Speaker, Mode::UrlWhole, Mode::UrlStreamed, Mode::Player];

struct FakeDevice {
    info: DeviceInfo,
    log: Log,
}

impl SatelliteDevice for FakeDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }
    async fn accept_run(&self) -> Result<()> {
        self.record("accept".into())
    }
    async fn refuse_run(&self) -> Result<()> {
        self.record("refuse".into())
    }
    async fn event(&self, event: Event, data: &[(&str, &str)]) -> Result<()> {
        let mut line = format!("{event:?}");
        for (key, value) in data {
            line += &format!(" {key}={value}");
        }
        self.record(line)
    }
    async fn audio(&self, pcm: &[i16]) -> Result<()> {
        self.record(format!("audio {}", pcm.len()))
    }
    async fn timer(&self, update: &TimerUpdate) -> Result<()> {
        self.record(format!("timer {} {}", update.event, update.timer_id))
    }
    async fn announce(&self, announce: &Announce) -> Result<()> {
        self.record(format!("announce {}", announce.media_id))
    }
}

impl FakeDevice {
    fn record(&self, line: String) -> Result<()> {
        self.log.lock().unwrap().push(clean(&line));
        Ok(())
    }
}

/// Wake word scores without a model: the first sample of each chunk, in thousandths.
struct Scores;

impl WakeScorer for Scores {
    fn reset(&mut self) {}
    fn push(&mut self, pcm: &[i16]) -> Result<Option<f32>> {
        Ok(Some(pcm.first().map_or(0.0, |&s| s as f32 / 1000.0)))
    }
}

/// Clip URLs hold the process id; the tests compare them without it.
fn clean(line: &str) -> String {
    line.replace(&format!("-{}", std::process::id()), "")
}

/// A Home Assistant on a loopback port that records each `play_media` call as "entity url volume".
fn fake_home_assistant() -> (String, Log) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Log::default();
    let calls = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut line, mut length) = (String::new(), 0);
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let data: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(path, "/api/services/media_player/play_media");
            let volume = data["extra"]["volume"].as_u64().map_or("-".into(), |v| v.to_string());
            let call = format!(
                "{} {} {volume}",
                data["entity_id"].as_str().unwrap(),
                data["media_content_id"].as_str().unwrap()
            );
            calls.lock().unwrap().push(clean(&call));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n[]");
        }
    });
    (base, log)
}

struct Harness {
    bridge: Bridge<'static, FakeDevice>,
    assistant: AssistantHandle,
    calls: Calls,
    device: Log,
    /// The fake Home Assistant's calls, in player mode.
    player: Log,
    /// Logs at info, as the engine runs by default.
    _log: tracing::subscriber::DefaultGuard,
}

impl Harness {
    /// A bridge whose device wakes on its own wake word, or streams for the assistant's name.
    fn new(mode: Mode) -> Self {
        let (answer_player, player) = match mode {
            Mode::Player => {
                let (base, log) = fake_home_assistant();
                let ha = HomeAssistant::new(&base, "token");
                (Some(Arc::new(AnswerPlayer { ha, entity: ENTITY.into(), volume: Some(0.4) })), log)
            }
            _ => (None, Log::default()),
        };
        let cfg = SatelliteConfig {
            address: "satellite.test:6053".into(),
            key: None,
            engine: "127.0.0.1:9".parse().unwrap(),
            public_url: None,
            http_port: 0,
            stream_answers: mode == Mode::UrlStreamed,
            answer_player,
            wake_words: Vec::new(),
            server_wake: None,
            dump: None,
        };
        let mut flags = feature::VOICE_ASSISTANT | feature::API_AUDIO | feature::ANNOUNCE;
        if mode == Mode::Speaker {
            flags |= feature::SPEAKER;
        }
        let device = Log::default();
        let fake = FakeDevice {
            info: DeviceInfo { voice_assistant_feature_flags: flags, ..DeviceInfo::default() },
            log: device.clone(),
        };
        let speech: &'static SpeechClips = Box::leak(Box::default());
        let bridge = Bridge::new(fake, &cfg, BASE.into(), speech);
        let (assistant, calls) = AssistantHandle::recorded_for_tests();
        let log = tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).with_test_writer().finish();
        let _log = tracing::subscriber::set_default(log);
        Self { bridge, assistant, calls, device, player, _log }
    }

    /// A bridge that scores the device's stream for the wake word: 0.5 wakes, and from 0.3 a near
    /// miss goes to the name check.
    fn server_wake(mode: Mode) -> Self {
        let mut h = Self::new(mode);
        h.bridge.server_wake = Some(WakeListener::new(Box::new(Scores), 0.5, Some(0.3)));
        h
    }

    fn wake(&self) -> &WakeListener {
        self.bridge.server_wake.as_ref().unwrap()
    }

    /// The answer's stream to the player is open.
    fn player_clip(&self) -> bool {
        matches!(&self.bridge.delivery, Delivery::Player { answer: Some(PlayerAnswer { clip: Some(_), .. }), .. })
    }

    fn take(&mut self) -> Vec<String> {
        let mut log = std::mem::take(&mut *self.device.lock().unwrap());
        log.extend(self.calls.take().into_iter().map(|call| format!("assistant: {call}")));
        log
    }

    async fn device(&mut self, message: Incoming) -> Vec<String> {
        self.bridge.device_message(message, &self.assistant).await.unwrap();
        self.take()
    }

    /// The device heard its own wake word.
    async fn start(&mut self) -> Vec<String> {
        let request =
            VoiceAssistantRequest { start: true, wake_word_phrase: "Hey Jarvis".into(), ..Default::default() };
        self.device(Incoming::Start(request)).await
    }

    /// The device streams for a wake word detected here.
    async fn stream(&mut self) -> Vec<String> {
        let request = VoiceAssistantRequest { start: true, flags: REQUEST_USE_WAKE_WORD, ..Default::default() };
        self.device(Incoming::Start(request)).await
    }

    async fn stop(&mut self) -> Vec<String> {
        self.device(Incoming::Stop).await
    }

    /// One chunk of microphone audio, with this wake word score.
    async fn audio(&mut self, score: f32) -> Vec<String> {
        self.device(Incoming::Audio(vec![(score * 1000.0).round() as i16; CHUNK])).await
    }

    async fn event(&mut self, event: AssistantEvent) -> Vec<String> {
        self.bridge.assistant_event(event).await.unwrap();
        self.take()
    }

    /// A request whose answer the worker expects to speak.
    async fn heard(&mut self, text: &str) -> Vec<String> {
        self.event(AssistantEvent::Heard { text: text.into(), words: true }).await
    }

    /// A request that may end without words ("pause the music").
    async fn heard_quiet(&mut self, text: &str) -> Vec<String> {
        self.event(AssistantEvent::Heard { text: text.into(), words: false }).await
    }

    async fn answer(&mut self, text: &str) -> Vec<String> {
        self.event(AssistantEvent::Answer { text: text.into() }).await
    }

    /// `seconds` of answer audio, 24 kHz.
    async fn speech(&mut self, seconds: f32) -> Vec<String> {
        let pcm: Arc<[i16]> = vec![1000; (seconds * SAMPLE_RATE as f32) as usize].into();
        self.event(AssistantEvent::Speech(pcm)).await
    }

    async fn spoken(&mut self) -> Vec<String> {
        self.event(AssistantEvent::Spoken).await
    }

    async fn tick(&mut self) -> Vec<String> {
        self.bridge.tick(&self.assistant).await.unwrap();
        self.take()
    }

    /// Time passes, then one tick.
    async fn after(&mut self, time: Duration) -> Vec<String> {
        tokio::time::advance(time).await;
        self.tick().await
    }

    /// The session ends (the device dropped the connection): the bridge is closed and dropped.
    fn disconnect(self) -> Vec<String> {
        let Self { bridge, assistant, mut calls, device, .. } = self;
        bridge.closed(&assistant);
        drop(bridge);
        drop(assistant);
        let mut log = std::mem::take(&mut *device.lock().unwrap());
        log.extend(calls.take().into_iter().map(|call| format!("assistant: {call}")));
        log
    }

    /// The player calls so far, once there are `n`: they are made on a blocking thread.
    fn announced(&mut self, n: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.player.lock().unwrap().len() < n && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::mem::take(&mut *self.player.lock().unwrap())
    }

    /// The device's run start, with the answer's URL for a device that streams it.
    fn run_start(&self, clip: u32) -> String {
        match matches!(self.bridge.delivery, Delivery::UrlStreamed(_)) {
            true => format!("RunStart url={BASE}/speech/{clip:x}.wav"),
            false => "RunStart".into(),
        }
    }
}

fn answer_on_player(clip: u32) -> String {
    format!("{ENTITY} {BASE}/speech/{clip:x}.wav 40")
}

// --- A normal run -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn speaker_answer_is_streamed_at_playback_pace() {
    let mut h = Harness::new(Mode::Speaker);
    assert_eq!(h.start().await, ["accept", "RunStart", "SttStart", "assistant: woke", "assistant: hear_now"]);
    assert_eq!(h.heard("turn on the lights").await, ["SttVadEnd", "SttEnd text=turn on the lights", "IntentStart"]);
    assert_eq!(
        h.answer("Okay, the lights are on.").await,
        ["IntentEnd continue_conversation=0", "TtsStart text=Okay, the lights are on.", "TtsStreamStart"]
    );
    assert_eq!(h.speech(1.0).await, NOTHING);
    // 0.4 s ahead of playback at once, the rest as it plays.
    assert_eq!(h.tick().await, ["audio 6400"]);
    assert_eq!(h.spoken().await, NOTHING);
    assert_eq!(h.after(Duration::from_millis(100)).await, ["audio 1600"]);
    assert_eq!(h.after(Duration::from_secs(1)).await, ["audio 7990", "TtsStreamEnd", "RunEnd"]);
    assert_eq!(h.after(Duration::from_secs(1)).await, NOTHING);
}

#[tokio::test(start_paused = true)]
async fn url_whole_answer_is_sent_once_spoken() {
    let mut h = Harness::new(Mode::UrlWhole);
    assert_eq!(h.start().await, ["accept", "RunStart", "SttStart", "assistant: woke", "assistant: hear_now"]);
    assert_eq!(h.heard("what time is it").await, ["SttVadEnd", "SttEnd text=what time is it", "IntentStart"]);
    assert_eq!(
        h.answer("It's ten past three.").await,
        ["IntentEnd continue_conversation=0", "TtsStart text=It's ten past three."]
    );
    assert_eq!(h.speech(0.5).await, NOTHING);
    assert_eq!(h.speech(0.5).await, NOTHING);
    assert_eq!(h.spoken().await, [format!("TtsEnd url={BASE}/speech/0.wav").as_str(), "RunEnd"]);
    assert_eq!(h.after(Duration::from_secs(1)).await, NOTHING);
}

#[tokio::test(start_paused = true)]
async fn url_streamed_answer_starts_with_its_first_audio() {
    let mut h = Harness::new(Mode::UrlStreamed);
    let url = format!("{BASE}/speech/0.wav");
    assert_eq!(
        h.start().await,
        ["accept".into(), h.run_start(0), "SttStart".into(), "assistant: woke".into(), "assistant: hear_now".into()]
    );
    assert_eq!(h.heard("what time is it").await, ["SttVadEnd", "SttEnd text=what time is it", "IntentStart"]);
    assert_eq!(
        h.answer("It's ten past three.").await,
        ["IntentEnd continue_conversation=0", "TtsStart text=It's ten past three."]
    );
    assert_eq!(h.speech(0.5).await, ["IntentProgress tts_start_streaming=1"]);
    assert_eq!(h.speech(0.5).await, NOTHING);
    assert_eq!(h.spoken().await, [format!("TtsEnd url={url}").as_str(), "RunEnd"]);
}

#[tokio::test(start_paused = true)]
async fn player_answer_is_announced_and_the_run_ends_when_spoken() {
    let mut h = Harness::new(Mode::Player);
    assert_eq!(h.start().await, ["accept", "RunStart", "SttStart", "assistant: woke", "assistant: hear_now"]);
    assert_eq!(h.heard("what time is it").await, ["SttVadEnd", "SttEnd text=what time is it", "IntentStart"]);
    // No speech events for the device: the answer goes to the player before its first audio.
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    assert_eq!(h.speech(1.0).await, NOTHING);
    assert_eq!(h.spoken().await, ["RunEnd"]);
    assert_eq!(h.after(Duration::from_secs(10)).await, NOTHING);
    assert_eq!(h.announced(0), NOTHING);
}

#[tokio::test(start_paused = true)]
async fn player_with_server_wake_streams_again_while_the_answer_plays() {
    let mut h = Harness::server_wake(Mode::Player);
    assert_eq!(h.stream().await, ["accept", "RunStart", "WakeWordStart"]);
    assert_eq!(h.audio(0.1).await, NOTHING);
    assert_eq!(
        h.audio(0.9).await,
        ["WakeWordEnd", "SttStart", "assistant: woke_on 3072 samples", "assistant: hear_now"]
    );
    assert_eq!(h.heard("what time is it").await, ["SttVadEnd", "SttEnd text=what time is it", "IntentStart"]);
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    // A run end while the device still stops its microphone is ignored by it.
    assert_eq!(h.after(Duration::from_millis(200)).await, NOTHING);
    assert_eq!(h.after(Duration::from_millis(50)).await, ["RunEnd"]);
    assert_eq!(h.stream().await, ["accept", "RunStart", "WakeWordStart"]);
    // The answer's audio still goes to the player's stream.
    assert_eq!(h.speech(1.0).await, NOTHING);
    assert_eq!(h.spoken().await, NOTHING);
    assert!(!h.player_clip());
}

#[tokio::test(start_paused = true)]
async fn the_assistant_hearing_its_name_starts_listening() {
    for mode in ALL {
        let mut h = Harness::new(mode);
        assert_eq!(h.stream().await, ["accept".into(), h.run_start(0), "WakeWordStart".into()], "{mode:?}");
        assert_eq!(h.audio(0.0).await, NOTHING, "{mode:?}");
        assert_eq!(h.event(AssistantEvent::Listening).await, ["WakeWordEnd", "SttStart"], "{mode:?}");
        assert_eq!(h.heard("what time is it").await, ["SttVadEnd", "SttEnd text=what time is it", "IntentStart"]);
    }
}

#[tokio::test(start_paused = true)]
async fn name_and_request_in_one_breath() {
    for mode in ALL {
        let mut h = Harness::new(mode);
        h.stream().await;
        assert_eq!(
            h.heard("jarvis what time is it").await,
            ["WakeWordEnd", "SttStart", "SttVadEnd", "SttEnd text=jarvis what time is it", "IntentStart"],
            "{mode:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn done_without_words_ends_the_run() {
    for mode in ALL {
        let mut h = Harness::new(mode);
        h.start().await;
        h.heard_quiet("pause the music").await;
        assert_eq!(h.event(AssistantEvent::Done).await, ["RunEnd"], "{mode:?}");
        assert_eq!(h.start().await[0], "accept", "{mode:?}");
    }
    // With the stream scored here, the run ends a moment later, as after a spoken answer.
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard_quiet("pause the music").await;
    assert_eq!(h.event(AssistantEvent::Done).await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.after(RUN_END_DELAY).await, ["RunEnd"]);
    assert_eq!(h.announced(0), NOTHING);
}

// --- The player's answer announced when its request is heard ---------------------------------

/// The player's answer stream, while it is open.
fn player_stream(h: &Harness) -> Option<Arc<Clip>> {
    match &h.bridge.delivery {
        Delivery::Player { answer: Some(answer), .. } => answer.clip.clone(),
        _ => None,
    }
}

/// A clip's audio, 24 kHz samples, and whether it is complete.
fn clip_audio(clip: &Clip) -> (usize, bool) {
    let (wav, done) = clip.read(0);
    ((wav.len() - 44) / 2, done)
}

#[tokio::test(start_paused = true)]
async fn player_answer_is_announced_when_the_request_is_heard() {
    let mut h = Harness::new(Mode::Player);
    h.start().await;
    // The device's events are as before; the player gets the answer's URL already.
    assert_eq!(h.heard("what time is it").await, ["SttVadEnd", "SttEnd text=what time is it", "IntentStart"]);
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    let clip = player_stream(&h).unwrap();
    assert!(!h.bridge.player_answer());
    // The words and their audio go into the stream already announced: no second call.
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.speech(1.0).await, NOTHING);
    assert!(Arc::ptr_eq(&clip, &player_stream(&h).unwrap()));
    assert_eq!(clip_audio(&clip), (SAMPLE_RATE as usize, false));
    assert_eq!(h.spoken().await, ["RunEnd"]);
    assert_eq!(clip_audio(&clip), (SAMPLE_RATE as usize, true));
    assert_eq!(h.announced(0), NOTHING);
}

#[tokio::test(start_paused = true)]
async fn player_with_server_wake_ends_the_device_run_when_the_answer_is_worded() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("what time is it").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    // Not at the announce: the device's run, and the gates on the answer, wait for its words.
    assert_eq!(h.after(Duration::from_secs(2)).await, NOTHING);
    assert!(!h.bridge.player_answer());
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    assert!(h.bridge.player_answer());
    assert_eq!(h.after(RUN_END_DELAY).await, ["RunEnd"]);
    assert_eq!(h.announced(0), NOTHING);
}

#[tokio::test(start_paused = true)]
async fn player_answer_announced_early_that_ends_silent_plays_a_moment_of_silence() {
    // Without the stream scored here: the run ends at once.
    let mut h = Harness::new(Mode::Player);
    h.start().await;
    h.heard("what is twelve divided by").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    let clip = player_stream(&h).unwrap();
    assert_eq!(h.event(AssistantEvent::Done).await, ["RunEnd"]);
    assert_eq!(clip_audio(&clip), (SILENCE, true));
    assert!(player_stream(&h).is_none());
    assert_eq!(h.start().await[0], "accept");

    // With it: a moment later, as after a spoken answer.
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("what is twelve divided by").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    let clip = player_stream(&h).unwrap();
    assert_eq!(h.event(AssistantEvent::Done).await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(clip_audio(&clip), (SILENCE, true));
    assert!(!h.bridge.player_answer());
    assert_eq!(h.after(RUN_END_DELAY).await, ["RunEnd"]);
    // Nothing plays, so the next wake word is no interrupt.
    assert_eq!(h.stream().await, ["accept", "RunStart", "WakeWordStart"]);
    assert_eq!(
        h.audio(0.9).await,
        ["WakeWordEnd", "SttStart", "assistant: woke_on 1536 samples", "assistant: hear_now"]
    );
    assert_eq!(h.announced(0), NOTHING);
}

#[tokio::test(start_paused = true)]
async fn player_answer_with_no_words_within_three_seconds_ends_as_silence() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("what time is it").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    let clip = player_stream(&h).unwrap();
    assert_eq!(h.after(WORDING_WAIT - TICK).await, NOTHING);
    assert_eq!(clip_audio(&clip), (0, false));
    assert_eq!(h.after(TICK).await, NOTHING);
    assert_eq!(clip_audio(&clip), (SILENCE, true));
    // Words that still come are announced on their own, as before.
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.announced(1), [answer_on_player(1)]);
    assert_eq!(h.speech(1.0).await, NOTHING);
    assert_eq!(clip_audio(&player_stream(&h).unwrap()), (SAMPLE_RATE as usize, false));
    assert_eq!(h.after(RUN_END_DELAY).await, ["RunEnd"]);
}

#[tokio::test(start_paused = true)]
async fn player_stop_and_wake_word_before_the_answer_is_worded_change_nothing() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("tell me a story").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    // The device's run goes on until the words come, so it does not stream for the wake word.
    assert_eq!(h.stop().await, NOTHING);
    assert_eq!(h.audio(0.9).await, NOTHING);
    assert_eq!(h.stream().await, ["refuse"]);
    assert!(player_stream(&h).is_some());
    h.answer("Once upon a time.").await;
    h.after(RUN_END_DELAY).await;
    h.stream().await;
    h.speech(1.0).await;
    // Once it is worded, the wake word over it interrupts it, as before.
    assert_eq!(
        h.audio(0.9).await,
        ["WakeWordEnd", "SttStart", "assistant: woke_on 1536 samples", "assistant: interrupt", "assistant: hear_now"]
    );
    assert_eq!(h.announced(1), [format!("{ENTITY} {BASE}/speech/1.wav 40")]);
    assert!(!h.player_clip());
}

#[tokio::test(start_paused = true)]
async fn player_stop_while_listening_announces_when_the_request_is_heard() {
    let mut h = Harness::new(Mode::Player);
    h.start().await;
    h.stop().await;
    assert_eq!(h.heard("what time is it").await, NOTHING);
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    h.speech(1.0).await;
    assert_eq!(h.spoken().await, ["RunEnd"]);
    assert_eq!(h.announced(0), NOTHING);
}

#[tokio::test(start_paused = true)]
async fn player_request_that_may_end_without_words_is_announced_only_with_words() {
    let mut h = Harness::new(Mode::Player);
    h.start().await;
    // The device's events are as for any request.
    assert_eq!(h.heard_quiet("pause the music").await, ["SttVadEnd", "SttEnd text=pause the music", "IntentStart"]);
    assert!(player_stream(&h).is_none());
    assert_eq!(h.announced(0), NOTHING);
    // Words after all ("Nothing is playing."): announced with them, as before.
    assert_eq!(h.answer("Nothing is playing.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.announced(1), [answer_on_player(0)]);

    // Done without words: nothing is announced.
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard_quiet("pause the music").await;
    assert_eq!(h.event(AssistantEvent::Done).await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.after(WORDING_WAIT).await, ["RunEnd"]);
    assert_eq!(h.announced(0), NOTHING);
}

#[tokio::test(start_paused = true)]
async fn player_follow_up_counts_from_the_answer_s_first_audio_when_it_comes_late() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("turn on the lights").await;
    let announced = Instant::now();
    h.after(Duration::from_secs(2)).await;
    h.answer("Which room?").await;
    h.speech(1.0).await;
    h.spoken().await;
    let played = announced + Duration::from_secs(2) + FIRST_AUDIO_LAG + Duration::from_secs(1);
    assert_eq!(h.bridge.follow_up_at, Some(played));

    // Audio that comes soon after the announce plays from the player's start.
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("turn on the lights").await;
    let announced = Instant::now();
    h.answer("Which room?").await;
    h.speech(1.0).await;
    h.spoken().await;
    assert_eq!(h.bridge.follow_up_at, Some(announced + PLAYER_START + Duration::from_secs(1)));
}

// --- A wake word over a playing answer --------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn speaker_refuses_a_run_while_it_streams_the_answer() {
    let mut h = Harness::new(Mode::Speaker);
    h.start().await;
    h.heard("tell me a story").await;
    h.answer("Once upon a time.").await;
    h.speech(2.0).await;
    h.tick().await;
    assert_eq!(h.start().await, ["refuse"]);
    assert_eq!(h.spoken().await, NOTHING);
    assert_eq!(h.after(Duration::from_secs(2)).await, ["audio 25590", "TtsStreamEnd", "RunEnd"]);
    assert_eq!(h.start().await, ["accept", "RunStart", "SttStart", "assistant: woke", "assistant: hear_now"]);
}

#[tokio::test(start_paused = true)]
async fn url_device_refuses_a_run_before_the_answer_is_spoken_and_takes_one_after() {
    for mode in [Mode::UrlWhole, Mode::UrlStreamed] {
        let mut h = Harness::new(mode);
        h.start().await;
        h.heard("tell me a story").await;
        h.answer("Once upon a time.").await;
        h.speech(1.0).await;
        assert_eq!(h.start().await, ["refuse"], "{mode:?}");
        assert_eq!(h.spoken().await.last().map(String::as_str), Some("RunEnd"), "{mode:?}");
        // The device plays the URL after its run has ended; its own wake word stops it there, and
        // the assistant is not told.
        assert_eq!(
            h.start().await,
            [
                "accept".into(),
                h.run_start(1),
                "SttStart".into(),
                "assistant: woke".into(),
                "assistant: hear_now".into()
            ],
            "{mode:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn player_without_server_wake_refuses_a_run_while_answering() {
    let mut h = Harness::new(Mode::Player);
    h.start().await;
    h.heard("tell me a story").await;
    h.answer("Once upon a time.").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    assert_eq!(h.start().await, ["refuse"]);
    assert_eq!(h.spoken().await, ["RunEnd"]);
    assert_eq!(h.start().await, ["accept", "RunStart", "SttStart", "assistant: woke", "assistant: hear_now"]);
    assert_eq!(h.announced(0), NOTHING);
}

#[tokio::test(start_paused = true)]
async fn player_wake_word_over_the_answer_interrupts_it() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("tell me a story").await;
    h.answer("Once upon a time.").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    h.after(RUN_END_DELAY).await;
    h.stream().await;
    h.speech(1.0).await;
    assert_eq!(
        h.audio(0.9).await,
        ["WakeWordEnd", "SttStart", "assistant: woke_on 1536 samples", "assistant: interrupt", "assistant: hear_now"]
    );
    // A moment of silence announced over the answer stops it.
    assert_eq!(h.announced(1), [format!("{ENTITY} {BASE}/speech/1.wav 40")]);
    assert!(!h.player_clip());
    // The rest of the interrupted answer goes nowhere.
    assert_eq!(h.speech(1.0).await, NOTHING);
    assert_eq!(h.spoken().await, NOTHING);
    assert_eq!(h.heard("what time is it").await, ["SttVadEnd", "SttEnd text=what time is it", "IntentStart"]);
}

#[tokio::test(start_paused = true)]
async fn player_wake_word_after_the_answer_was_spoken_but_still_plays_interrupts_it() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("tell me a story").await;
    h.answer("Once upon a time.").await;
    h.after(RUN_END_DELAY).await;
    h.stream().await;
    h.speech(3.0).await;
    h.spoken().await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    assert_eq!(h.after(Duration::from_secs(1)).await, NOTHING);
    assert_eq!(
        h.audio(0.9).await,
        ["WakeWordEnd", "SttStart", "assistant: woke_on 1536 samples", "assistant: interrupt", "assistant: hear_now"]
    );
    assert_eq!(h.announced(1), [format!("{ENTITY} {BASE}/speech/1.wav 40")]);
    assert_eq!(h.bridge.follow_up_at, None);
}

/// An answer on the player, with the device streaming for the wake word again under it.
async fn story_playing(h: &mut Harness) {
    h.stream().await;
    h.audio(0.9).await;
    h.heard("tell me a story").await;
    h.answer("Once upon a time.").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    h.after(RUN_END_DELAY).await;
    h.stream().await;
    h.speech(1.0).await;
    h.take();
}

#[tokio::test(start_paused = true)]
async fn player_button_over_the_answer_stops_it() {
    let mut h = Harness::server_wake(Mode::Player);
    story_playing(&mut h).await;
    // The button: the device stops its stream and starts a run without the wake word.
    h.stop().await;
    assert_eq!(h.start().await, ["accept", "RunStart", "RunEnd", "assistant: stop by button", "assistant: interrupt"]);
    assert_eq!(h.announced(1), [format!("{ENTITY} {BASE}/speech/1.wav 40")]);
    assert!(!h.player_clip());
    // No follow-up window after it.
    assert_eq!(h.spoken().await, NOTHING);
    assert_eq!(h.bridge.follow_up_at, None);
    h.stream().await;
    assert_eq!(h.after(Duration::from_secs(10)).await, NOTHING);
}

#[tokio::test(start_paused = true)]
async fn player_button_with_nothing_playing_starts_a_request() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.stop().await;
    assert_eq!(h.start().await, ["accept", "RunStart", "SttStart", "assistant: woke", "assistant: hear_now"]);
}

/// With `dump`, what the microphone heard while the answer played is kept: the echo, and anyone
/// talking over it.
#[tokio::test(start_paused = true)]
async fn player_answer_heard_over_is_saved_once_played() {
    let dir = std::env::temp_dir().join(format!("answer-heard-{}", std::process::id()));
    let mut h = Harness::server_wake(Mode::Player);
    h.bridge.dump = Some(dir.clone());
    story_playing(&mut h).await;
    h.audio(0.0).await;
    h.audio(0.0).await;
    assert!(!h.bridge.answer_heard.is_empty());
    // Synthesized, but still on the player: what it plays from here on is kept too.
    h.spoken().await;
    let before = h.bridge.answer_heard.len();
    h.audio(0.0).await;
    assert!(h.bridge.answer_heard.len() > before);
    h.after(Duration::from_secs(10)).await;
    assert!(h.bridge.answer_heard.is_empty());
    let saved: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("answer-"))
        .collect();
    assert_eq!(saved.len(), 1, "{saved:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// "Jarvis, stop" under the loud player scores under the threshold: the near miss's name check
/// hears it.
#[tokio::test(start_paused = true)]
async fn player_name_and_stop_over_the_answer_stops_it() {
    let mut h = Harness::server_wake(Mode::Player);
    story_playing(&mut h).await;
    assert_eq!(h.audio(0.4).await, NOTHING);
    assert_eq!(h.audio(0.25).await, ["assistant: check_name 3072 samples"]);
    h.calls.name_checks.pop_front().unwrap().send(Some(NameHeard { named: true, stop: true })).unwrap();
    assert_eq!(h.audio(0.0).await, ["assistant: stop by name", "assistant: interrupt"]);
    assert_eq!(h.announced(1), [format!("{ENTITY} {BASE}/speech/1.wav 40")]);
    assert_eq!(h.spoken().await, NOTHING);
    assert_eq!(h.bridge.follow_up_at, None);
}

/// The name alone over the answer, scored low: as a full wake word, it stops the answer and
/// listens.
#[tokio::test(start_paused = true)]
async fn player_name_over_the_answer_interrupts_it_and_listens() {
    let mut h = Harness::server_wake(Mode::Player);
    story_playing(&mut h).await;
    h.audio(0.4).await;
    assert_eq!(h.audio(0.25).await, ["assistant: check_name 3072 samples"]);
    h.calls.name_checks.pop_front().unwrap().send(Some(NameHeard { named: true, stop: false })).unwrap();
    assert_eq!(
        h.audio(0.0).await,
        ["WakeWordEnd", "SttStart", "assistant: woke_named", "assistant: interrupt", "assistant: hear_now"]
    );
    assert_eq!(h.announced(1), [format!("{ENTITY} {BASE}/speech/1.wav 40")]);
}

// --- The follow-up window ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn device_question_makes_the_device_listen_again() {
    for mode in [Mode::Speaker, Mode::UrlWhole, Mode::UrlStreamed] {
        let mut h = Harness::new(mode);
        h.start().await;
        h.heard("turn on the lights").await;
        let answer = h.answer("Which room?").await;
        assert_eq!(answer[0], "IntentEnd continue_conversation=1", "{mode:?}");
        h.speech(0.5).await;
        h.spoken().await;
        h.after(Duration::from_secs(1)).await;
        // The device starts the next run itself, without its wake word, once it has spoken.
        assert_eq!(
            h.start().await,
            [
                "accept".into(),
                h.run_start(1),
                "SttStart".into(),
                "assistant: woke".into(),
                "assistant: hear_now".into()
            ],
            "{mode:?}"
        );
        assert_eq!(h.heard("the kitchen").await, ["SttVadEnd", "SttEnd text=the kitchen", "IntentStart"], "{mode:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn device_follow_up_with_nothing_said_ends_at_the_listen_timeout() {
    for mode in [Mode::Speaker, Mode::UrlWhole, Mode::UrlStreamed] {
        let mut h = Harness::new(mode);
        h.start().await;
        h.heard("turn on the lights").await;
        h.answer("Which room?").await;
        h.speech(0.5).await;
        h.spoken().await;
        h.after(Duration::from_secs(1)).await;
        h.start().await;
        assert_eq!(h.after(LISTEN_TIMEOUT).await, NOTHING, "{mode:?}");
        assert_eq!(h.after(TICK).await, ["RunEnd", "assistant: gave_up"], "{mode:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn player_question_does_not_continue_the_conversation_on_the_device() {
    for mut h in [Harness::new(Mode::Player), Harness::server_wake(Mode::Player)] {
        h.start().await;
        h.heard("turn on the lights").await;
        assert_eq!(h.answer("Which room?").await, ["IntentEnd continue_conversation=0"]);
    }
}

/// Wake, request, answer on the player, the device streaming again, and 1 s of answer spoken:
/// the follow-up window opens 1.8 s after the answer was announced.
async fn player_answered(h: &mut Harness) {
    h.stream().await;
    h.audio(0.9).await;
    h.heard("turn on the lights").await;
    h.answer("Which room?").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    h.after(RUN_END_DELAY).await;
    h.stream().await;
    h.speech(1.0).await;
    h.spoken().await;
}

#[tokio::test(start_paused = true)]
async fn player_follow_up_listens_without_the_wake_word_once_the_answer_has_played() {
    let mut h = Harness::server_wake(Mode::Player);
    player_answered(&mut h).await;
    let opens = PLAYER_START + Duration::from_secs(1) - RUN_END_DELAY;
    assert_eq!(h.after(opens - TICK).await, NOTHING);
    assert_eq!(h.after(TICK).await, ["SttStart", "assistant: hear_now"]);
    assert_eq!(h.heard("the kitchen").await, ["SttVadEnd", "SttEnd text=the kitchen", "IntentStart"]);
    assert_eq!(h.answer("Okay, the kitchen lights are on.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.announced(1), [answer_on_player(1)]);
}

#[tokio::test(start_paused = true)]
async fn player_follow_up_window_closes_after_six_seconds() {
    let mut h = Harness::server_wake(Mode::Player);
    player_answered(&mut h).await;
    let opens = PLAYER_START + Duration::from_secs(1) - RUN_END_DELAY;
    assert_eq!(h.after(opens).await, ["SttStart", "assistant: hear_now"]);
    assert_eq!(h.after(FOLLOW_UP).await, NOTHING);
    assert_eq!(h.after(TICK).await, ["RunEnd"]);
    assert_eq!(h.heard("the kitchen").await, NOTHING);
    assert_eq!(h.announced(0), NOTHING);
}

/// 23 November 2026: "No I'm feeling sick like I'm not feeling well." went on past the window, which
/// closed in the middle of it.
#[tokio::test(start_paused = true)]
async fn player_follow_up_window_stays_open_while_a_reply_is_said() {
    let mut h = Harness::server_wake(Mode::Player);
    player_answered(&mut h).await;
    let opens = PLAYER_START + Duration::from_secs(1) - RUN_END_DELAY;
    assert_eq!(h.after(opens).await, ["SttStart", "assistant: hear_now"]);
    h.assistant.set_speaking_for_tests(true);
    assert_eq!(h.after(FOLLOW_UP + Duration::from_secs(3)).await, NOTHING);
    h.assistant.set_speaking_for_tests(false);
    assert_eq!(h.after(TICK).await, ["RunEnd"]);
}

/// The TV talking on does not keep the window open.
#[tokio::test(start_paused = true)]
async fn player_follow_up_window_closes_on_speech_that_goes_on_and_on() {
    let mut h = Harness::server_wake(Mode::Player);
    player_answered(&mut h).await;
    let opens = PLAYER_START + Duration::from_secs(1) - RUN_END_DELAY;
    assert_eq!(h.after(opens).await, ["SttStart", "assistant: hear_now"]);
    h.assistant.set_speaking_for_tests(true);
    assert_eq!(h.after(FOLLOW_UP + FOLLOW_UP_SPEECH).await, NOTHING);
    assert_eq!(h.after(TICK).await, ["RunEnd"]);
}

#[tokio::test(start_paused = true)]
async fn player_stale_follow_up_deadline_does_not_open_the_window_later() {
    let mut h = Harness::server_wake(Mode::Player);
    player_answered(&mut h).await;
    // The device leaves its stream before the window opens, and comes back much later.
    assert_eq!(h.stop().await, ["RunEnd"]);
    assert_eq!(h.after(Duration::from_secs(30)).await, NOTHING);
    h.stream().await;
    assert_eq!(h.tick().await, NOTHING);
}

// --- A stop from the device -------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn stop_while_listening_ends_the_run_on_a_device() {
    for mode in [Mode::Speaker, Mode::UrlWhole, Mode::UrlStreamed] {
        let mut h = Harness::new(mode);
        h.start().await;
        assert_eq!(h.stop().await, ["RunEnd", "assistant: gave_up"], "{mode:?}");
        // What the assistant then hears is not shown on the device.
        assert_eq!(h.heard("what time is it").await, NOTHING, "{mode:?}");
        assert_eq!(h.answer("It's ten past three.").await, NOTHING, "{mode:?}");
        assert_eq!(h.start().await[0], "accept", "{mode:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn stop_while_listening_tells_the_assistant() {
    for mode in [Mode::Speaker, Mode::UrlWhole, Mode::UrlStreamed] {
        let mut h = Harness::new(mode);
        h.start().await;
        assert_eq!(h.stop().await, ["RunEnd", "assistant: gave_up"], "{mode:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn player_stop_while_listening_still_answers_on_the_player() {
    let mut h = Harness::new(Mode::Player);
    h.start().await;
    assert_eq!(h.stop().await, ["RunEnd"]);
    // The request was said: the device's end of speech came first.
    assert_eq!(h.heard("what time is it").await, NOTHING);
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    assert_eq!(h.speech(1.0).await, NOTHING);
    // The device's run already ended, and it is ended again.
    assert_eq!(h.spoken().await, ["RunEnd"]);
}

#[tokio::test(start_paused = true)]
async fn player_stop_while_listening_with_nothing_heard_goes_idle_at_the_timeout() {
    let mut h = Harness::new(Mode::Player);
    h.start().await;
    h.stop().await;
    assert_eq!(h.after(LISTEN_TIMEOUT).await, NOTHING);
    assert_eq!(h.after(TICK).await, ["assistant: gave_up"]);
    assert_eq!(h.heard("what time is it").await, NOTHING);
    assert_eq!(h.start().await[0], "accept");
}

#[tokio::test(start_paused = true)]
async fn player_stop_while_listening_with_nothing_heard_tells_the_assistant() {
    let mut h = Harness::new(Mode::Player);
    h.start().await;
    h.stop().await;
    h.after(LISTEN_TIMEOUT).await;
    assert_eq!(h.after(TICK).await, ["assistant: gave_up"]);
}

#[tokio::test(start_paused = true)]
async fn player_stop_then_done_ends_quietly() {
    let mut h = Harness::new(Mode::Player);
    h.start().await;
    h.stop().await;
    assert_eq!(h.event(AssistantEvent::Done).await, NOTHING);
    assert_eq!(h.after(LISTEN_TIMEOUT * 2).await, NOTHING);
}

#[tokio::test(start_paused = true)]
async fn stop_while_answering_changes_nothing() {
    let mut h = Harness::new(Mode::Speaker);
    h.start().await;
    h.heard("what time is it").await;
    assert_eq!(h.stop().await, NOTHING);
    h.answer("It's ten past three.").await;
    h.speech(0.2).await;
    h.spoken().await;
    assert_eq!(h.tick().await, ["audio 3190", "TtsStreamEnd", "RunEnd"]);

    for mode in [Mode::UrlWhole, Mode::UrlStreamed] {
        let mut h = Harness::new(mode);
        h.start().await;
        h.heard("what time is it").await;
        assert_eq!(h.stop().await, NOTHING, "{mode:?}");
        assert_eq!(h.answer("It's ten past three.").await.len(), 2, "{mode:?}");
        h.speech(0.2).await;
        assert_eq!(h.spoken().await.last().map(String::as_str), Some("RunEnd"), "{mode:?}");
    }

    let mut h = Harness::new(Mode::Player);
    h.start().await;
    h.heard("what time is it").await;
    assert_eq!(h.stop().await, NOTHING);
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    assert_eq!(h.spoken().await, ["RunEnd"]);
}

#[tokio::test(start_paused = true)]
async fn player_with_server_wake_stop_while_answering_changes_nothing() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("what time is it").await;
    assert_eq!(h.stop().await, NOTHING);
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    assert_eq!(h.after(RUN_END_DELAY).await, ["RunEnd"]);
}

#[tokio::test(start_paused = true)]
async fn player_stop_then_stream_again_with_nothing_heard_tells_the_assistant_once() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    assert_eq!(h.stop().await, ["RunEnd"]);
    // The device streams for the wake word again before the request was heard.
    assert_eq!(h.stream().await, ["accept", "RunStart", "WakeWordStart"]);
    assert_eq!(h.after(LISTEN_TIMEOUT).await, NOTHING);
    assert_eq!(h.after(TICK).await, ["assistant: gave_up"]);
    assert_eq!(h.after(LISTEN_TIMEOUT).await, NOTHING);
    assert_eq!(h.disconnect(), NOTHING);
}

#[tokio::test(start_paused = true)]
async fn player_stop_then_stream_again_still_answers_a_request_heard_later() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.stop().await;
    h.stream().await;
    assert_eq!(
        h.heard("what time is it").await,
        ["WakeWordEnd", "SttStart", "SttVadEnd", "SttEnd text=what time is it", "IntentStart"]
    );
    assert_eq!(h.answer("It's ten past three.").await, ["IntentEnd continue_conversation=0"]);
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    assert_eq!(h.after(LISTEN_TIMEOUT + TICK).await, ["RunEnd"]);
}

// --- No request after the wake word -----------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn listen_timeout_gives_up() {
    for mode in ALL {
        let mut h = Harness::new(mode);
        h.start().await;
        assert_eq!(h.after(LISTEN_TIMEOUT).await, NOTHING, "{mode:?}");
        assert_eq!(h.after(TICK).await, ["RunEnd", "assistant: gave_up"], "{mode:?}");
        assert_eq!(h.heard("what time is it").await, NOTHING, "{mode:?}");
    }
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.after(LISTEN_TIMEOUT).await;
    assert_eq!(h.after(TICK).await, ["RunEnd", "assistant: gave_up"]);
}

// --- The session ends -------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn disconnect_while_listening_tells_the_assistant() {
    for mode in ALL {
        let mut h = Harness::new(mode);
        h.start().await;
        assert_eq!(h.disconnect(), ["assistant: gave_up"], "{mode:?}");
    }
}

// --- A lost answer event ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn lost_spoken_event_does_not_block_later_runs() {
    for mode in ALL {
        let mut h = Harness::new(mode);
        h.start().await;
        h.heard("what time is it").await;
        h.answer("It's ten past three.").await;
        h.speech(0.5).await;
        h.after(Duration::from_secs(300)).await;
        assert_eq!(h.start().await.first().map(String::as_str), Some("accept"), "{mode:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn lost_spoken_event_ends_the_run_after_a_minute_of_quiet() {
    for mode in ALL {
        let mut h = Harness::new(mode);
        h.start().await;
        h.heard("what time is it").await;
        h.answer("It's ten past three.").await;
        h.speech(0.5).await;
        // A speaker device has had all of the audio by then.
        h.after(Duration::from_secs(1)).await;
        assert_eq!(h.after(ANSWER_SILENCE - Duration::from_secs(2)).await, NOTHING, "{mode:?}");
        assert_eq!(h.after(Duration::from_secs(2) + TICK).await, ["RunEnd"], "{mode:?}");
        assert_eq!(h.after(ANSWER_SILENCE).await, NOTHING, "{mode:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_request_long_after_the_last_answer_is_answered() {
    for mode in ALL {
        let mut h = Harness::new(mode);
        h.start().await;
        h.heard("what time is it").await;
        h.answer("It's ten past three.").await;
        h.speech(0.5).await;
        h.spoken().await;
        h.after(Duration::from_secs(300)).await;
        h.start().await;
        h.heard("what time is it").await;
        // The minute of quiet counts from this request, not from the last answer's speech.
        assert!(!h.after(TICK).await.iter().any(|e| e == "RunEnd"), "{mode:?}");
        assert!(h.bridge.run == Run::Answering, "{mode:?}");
        assert!(!h.answer("It's twenty past three.").await.is_empty(), "{mode:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn speaker_answer_still_playing_is_not_quiet() {
    let mut h = Harness::new(Mode::Speaker);
    h.start().await;
    h.heard("tell me a story").await;
    h.answer("Once upon a time.").await;
    h.speech(90.0).await;
    // 90 s of audio, sent as it plays: the minute without events is not a lost end.
    assert_eq!(h.after(ANSWER_SILENCE + TICK).await.len(), 1);
    assert_eq!(h.after(Duration::from_secs(30)).await.len(), 1);
    assert!(matches!(&h.bridge.delivery, Delivery::Speaker(stream) if stream.outgoing.is_empty()));
    // The quiet minute counts from the end of the audio sent.
    assert_eq!(h.after(ANSWER_SILENCE - Duration::from_secs(1)).await, NOTHING);
    assert!(h.bridge.run == Run::Answering);
    assert_eq!(h.after(Duration::from_secs(1) + TICK).await, ["RunEnd"]);
}

#[tokio::test(start_paused = true)]
async fn player_with_server_wake_lost_spoken_event_does_not_interrupt_the_next_wake() {
    // The device's run ends without the event; the watchdog stops counting the answer as playing.
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    h.audio(0.9).await;
    h.heard("what time is it").await;
    h.answer("It's ten past three.").await;
    assert_eq!(h.announced(1), [answer_on_player(0)]);
    assert_eq!(h.after(RUN_END_DELAY).await, ["RunEnd"]);
    h.stream().await;
    assert_eq!(h.after(Duration::from_secs(300)).await, NOTHING);
    assert!(!h.bridge.player_answer());
    assert!(!h.player_clip());
    assert_eq!(
        h.audio(0.9).await,
        ["WakeWordEnd", "SttStart", "assistant: woke_on 1536 samples", "assistant: hear_now"]
    );
    assert_eq!(h.announced(0), NOTHING);
}

// --- Server-side wake word: near misses and the name check ------------------------------------

/// Scores that peak at 0.4, under the threshold but over `check_from`: the peak's audio goes to
/// the name check once the scores fall.
async fn near_miss(h: &mut Harness) {
    assert_eq!(h.stream().await, ["accept".into(), h.run_start(0), "WakeWordStart".into()]);
    assert_eq!(h.audio(0.35).await, NOTHING);
    assert_eq!(h.audio(0.4).await, NOTHING);
    assert_eq!(h.audio(0.25).await, ["assistant: check_name 4608 samples"]);
}

#[tokio::test(start_paused = true)]
async fn near_miss_with_the_name_wakes() {
    for mode in ALL {
        let mut h = Harness::server_wake(mode);
        near_miss(&mut h).await;
        assert_eq!(h.audio(0.0).await, NOTHING, "{mode:?}");
        h.calls.name_checks.pop_front().unwrap().send(Some(NameHeard { named: true, stop: false })).unwrap();
        assert_eq!(
            h.audio(0.0).await,
            ["WakeWordEnd", "SttStart", "assistant: woke_named", "assistant: hear_now"],
            "{mode:?}"
        );
        // The audio heard while the name was checked goes on to the engine: 2 chunks at 24 kHz,
        // less the resampler's delay.
        assert_eq!(h.bridge.mic.len(), 2 * CHUNK * 3 / 2 - 24, "{mode:?}");
        assert_eq!(h.heard("what time is it").await, ["SttVadEnd", "SttEnd text=what time is it", "IntentStart"]);
    }
}

#[tokio::test(start_paused = true)]
async fn near_miss_without_the_name_keeps_streaming() {
    let mut h = Harness::server_wake(Mode::Player);
    near_miss(&mut h).await;
    h.calls.name_checks.pop_front().unwrap().send(Some(NameHeard { named: false, stop: false })).unwrap();
    assert_eq!(h.audio(0.0).await, NOTHING);
    assert!(h.wake().checking.is_none());
    assert!(h.bridge.mic.is_empty());
    // A new run of near misses is checked again.
    assert_eq!(h.audio(0.4).await, NOTHING);
    assert_eq!(h.audio(0.3).await, ["assistant: check_name 9216 samples"]);
}

#[tokio::test(start_paused = true)]
async fn near_miss_without_the_name_is_cleared_with_logging_off() {
    let mut h = Harness::server_wake(Mode::Player);
    let _off =
        tracing::subscriber::set_default(tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).finish());
    near_miss(&mut h).await;
    h.calls.name_checks.pop_front().unwrap().send(Some(NameHeard { named: false, stop: false })).unwrap();
    assert_eq!(h.audio(0.0).await, NOTHING);
    assert!(h.wake().checking.is_none());
}

#[tokio::test(start_paused = true)]
async fn near_miss_check_that_cannot_transcribe_keeps_streaming() {
    let mut h = Harness::server_wake(Mode::Player);
    near_miss(&mut h).await;
    h.calls.name_checks.pop_front().unwrap().send(None).unwrap();
    assert_eq!(h.audio(0.0).await, NOTHING);
    assert!(h.wake().checking.is_none());
}

#[tokio::test(start_paused = true)]
async fn near_miss_check_that_does_not_answer_lets_the_wake_word_go() {
    let mut h = Harness::server_wake(Mode::Player);
    near_miss(&mut h).await;
    tokio::time::advance(CHECK_TIMEOUT - TICK).await;
    assert_eq!(h.audio(0.0).await, NOTHING);
    assert!(h.wake().checking.is_some());
    tokio::time::advance(TICK).await;
    assert_eq!(h.audio(0.0).await, NOTHING);
    assert!(h.wake().checking.is_none());
    let late = h.calls.name_checks.pop_front().unwrap();
    assert!(late.send(Some(NameHeard { named: true, stop: false })).is_err());
    assert_eq!(h.audio(0.0).await, NOTHING);
}

#[tokio::test(start_paused = true)]
async fn near_miss_under_check_from_is_only_logged() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    for score in [0.22, 0.28, 0.21, 0.1, 0.0] {
        assert_eq!(h.audio(score).await, NOTHING);
    }
    assert_eq!(h.wake().near_miss, 0.0);
}

#[tokio::test(start_paused = true)]
async fn near_miss_is_not_carried_into_the_next_stream() {
    let mut h = Harness::server_wake(Mode::Player);
    h.stream().await;
    assert_eq!(h.audio(0.35).await, NOTHING);
    assert_eq!(h.audio(0.4).await, NOTHING);
    assert_eq!(h.stop().await, ["RunEnd"]);
    h.stream().await;
    assert_eq!(h.audio(0.0).await, NOTHING);
}

#[tokio::test(start_paused = true)]
async fn wake_word_during_a_name_check_wakes_at_once() {
    let mut h = Harness::server_wake(Mode::Player);
    near_miss(&mut h).await;
    assert_eq!(
        h.audio(0.8).await,
        ["WakeWordEnd", "SttStart", "assistant: woke_on 6144 samples", "assistant: hear_now"]
    );
    assert!(h.wake().checking.is_none());
}

#[tokio::test(start_paused = true)]
async fn paused_assistant_scores_nothing() {
    let mut h = Harness::server_wake(Mode::Player);
    h.assistant.pause.pause(None);
    h.stream().await;
    assert_eq!(h.audio(0.9).await, NOTHING);
    // The device's own wake word starts nothing either.
    assert_eq!(h.start().await, ["refuse"]);
}

// --- Announcements and timers -----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn announcements_play_where_answers_play() {
    let announcement = |on| AssistantEvent::Announcement {
        text: "The timer is done.".into(),
        audio: vec![0; 2400].into(),
        timer_ring: false,
        on,
    };
    for mode in [Mode::Speaker, Mode::UrlWhole, Mode::UrlStreamed] {
        let mut h = Harness::new(mode);
        assert_eq!(h.event(announcement(AnnounceOn::AnswerPlayer)).await, [format!("announce {BASE}/speech/0.wav")]);
    }
    let mut h = Harness::new(Mode::Player);
    assert_eq!(h.event(announcement(AnnounceOn::Device)).await, [format!("announce {BASE}/speech/0.wav")]);
    assert_eq!(h.event(announcement(AnnounceOn::AnswerPlayer)).await, NOTHING);
    assert_eq!(h.announced(1), [answer_on_player(1)]);
    assert_eq!(h.event(announcement(AnnounceOn::Player("media_player.kitchen".into()))).await, NOTHING);
    assert_eq!(h.announced(1), [format!("media_player.kitchen {BASE}/speech/2.wav 40")]);
}
