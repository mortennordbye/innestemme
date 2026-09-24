//! The protobuf subset the voice path needs: varints, length-delimited fields, and the ESPHome API
//! messages for the connection and the voice assistant. Hand-rolled to avoid a protoc build step;
//! field numbers follow `esphome/components/api/api.proto`.

use anyhow::{bail, Result};

/// Message type ids from `api.proto` (`option (id)`).
pub mod id {
    pub const HELLO_REQUEST: u16 = 1;
    pub const HELLO_RESPONSE: u16 = 2;
    pub const DISCONNECT_REQUEST: u16 = 5;
    pub const DISCONNECT_RESPONSE: u16 = 6;
    pub const PING_REQUEST: u16 = 7;
    pub const PING_RESPONSE: u16 = 8;
    pub const DEVICE_INFO_REQUEST: u16 = 9;
    pub const DEVICE_INFO_RESPONSE: u16 = 10;
    pub const SUBSCRIBE_VOICE_ASSISTANT_REQUEST: u16 = 89;
    pub const VOICE_ASSISTANT_REQUEST: u16 = 90;
    pub const VOICE_ASSISTANT_RESPONSE: u16 = 91;
    pub const VOICE_ASSISTANT_EVENT_RESPONSE: u16 = 92;
    pub const VOICE_ASSISTANT_AUDIO: u16 = 106;
    pub const VOICE_ASSISTANT_TIMER_EVENT_RESPONSE: u16 = 115;
    pub const VOICE_ASSISTANT_ANNOUNCE_REQUEST: u16 = 119;
    pub const VOICE_ASSISTANT_ANNOUNCE_FINISHED: u16 = 120;
}

/// `DeviceInfoResponse.voice_assistant_feature_flags` (aioesphomeapi `VoiceAssistantFeature`).
pub mod feature {
    pub const VOICE_ASSISTANT: u32 = 1 << 0;
    /// Plays response audio streamed over the API; without it the device fetches a URL.
    pub const SPEAKER: u32 = 1 << 1;
    pub const API_AUDIO: u32 = 1 << 2;
    pub const TIMERS: u32 = 1 << 3;
    pub const ANNOUNCE: u32 = 1 << 4;
    pub const START_CONVERSATION: u32 = 1 << 5;
}

/// `SubscribeVoiceAssistantRequest.flags`: audio over the API connection instead of UDP.
pub const SUBSCRIBE_API_AUDIO: u32 = 1;
/// `VoiceAssistantRequest.flags`.
pub const REQUEST_USE_VAD: u32 = 1;
/// The device wants the wake word detected on the server (continuous streaming).
pub const REQUEST_USE_WAKE_WORD: u32 = 2;

/// `VoiceAssistantEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Event {
    Error = 0,
    RunStart = 1,
    RunEnd = 2,
    SttStart = 3,
    SttEnd = 4,
    IntentStart = 5,
    IntentEnd = 6,
    TtsStart = 7,
    TtsEnd = 8,
    WakeWordStart = 9,
    WakeWordEnd = 10,
    SttVadStart = 11,
    SttVadEnd = 12,
    TtsStreamStart = 98,
    TtsStreamEnd = 99,
    IntentProgress = 100,
}

/// `VoiceAssistantTimerEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TimerEvent {
    Started = 0,
    Updated = 1,
    Cancelled = 2,
    Finished = 3,
}

// --- Encoding ---------------------------------------------------------------------------------

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn key(&mut self, field: u32, wire: u8) {
        self.varint(((field as u64) << 3) | wire as u64);
    }

    pub fn uint(&mut self, field: u32, v: u64) -> &mut Self {
        if v != 0 {
            self.key(field, 0);
            self.varint(v);
        }
        self
    }

    pub fn bool(&mut self, field: u32, v: bool) -> &mut Self {
        self.uint(field, v as u64)
    }

    pub fn bytes(&mut self, field: u32, v: &[u8]) -> &mut Self {
        if !v.is_empty() {
            self.key(field, 2);
            self.varint(v.len() as u64);
            self.0.extend_from_slice(v);
        }
        self
    }

    pub fn str(&mut self, field: u32, v: &str) -> &mut Self {
        self.bytes(field, v.as_bytes())
    }

    /// An embedded message, written even when empty (repeated entries must not vanish).
    pub fn message(&mut self, field: u32, v: &[u8]) -> &mut Self {
        self.key(field, 2);
        self.varint(v.len() as u64);
        self.0.extend_from_slice(v);
        self
    }

    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

pub fn varint(v: u64) -> Vec<u8> {
    let mut w = Writer::default();
    w.varint(v);
    w.0
}

// --- Decoding ---------------------------------------------------------------------------------

pub enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
    Fixed64(u64),
}

impl Value<'_> {
    pub fn uint(&self) -> u64 {
        match *self {
            Value::Varint(v) | Value::Fixed64(v) => v,
            Value::Fixed32(v) => v as u64,
            Value::Bytes(_) => 0,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Value::Bytes(b) => b,
            _ => &[],
        }
    }

    pub fn string(&self) -> String {
        String::from_utf8_lossy(self.bytes()).into_owned()
    }
}

/// Reads a varint at `pos`; the value and the position after it.
pub fn read_varint(buf: &[u8], mut pos: usize) -> Result<(u64, usize)> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let Some(&b) = buf.get(pos) else { bail!("truncated varint") };
        pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok((v, pos));
        }
    }
    bail!("varint too long")
}

/// Every field of a message, in order.
pub fn fields(buf: &[u8]) -> Result<Vec<(u32, Value<'_>)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let (key, next) = read_varint(buf, pos)?;
        pos = next;
        let field = (key >> 3) as u32;
        let value = match key & 7 {
            0 => {
                let (v, next) = read_varint(buf, pos)?;
                pos = next;
                Value::Varint(v)
            }
            1 => {
                let Some(b) = buf.get(pos..pos + 8) else { bail!("truncated fixed64") };
                pos += 8;
                Value::Fixed64(u64::from_le_bytes(b.try_into()?))
            }
            2 => {
                let (len, next) = read_varint(buf, pos)?;
                let end = next + len as usize;
                let Some(b) = buf.get(next..end) else { bail!("truncated field {field}") };
                pos = end;
                Value::Bytes(b)
            }
            5 => {
                let Some(b) = buf.get(pos..pos + 4) else { bail!("truncated fixed32") };
                pos += 4;
                Value::Fixed32(u32::from_le_bytes(b.try_into()?))
            }
            wire => bail!("unsupported wire type {wire} in field {field}"),
        };
        out.push((field, value));
    }
    Ok(out)
}

// --- Messages ---------------------------------------------------------------------------------

pub fn hello_request(client_info: &str) -> Vec<u8> {
    // API 1.10: what current aioesphomeapi sends.
    Writer::default().str(1, client_info).uint(2, 1).uint(3, 10).finish()
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct HelloResponse {
    pub api_major: u32,
    pub api_minor: u32,
    pub server_info: String,
    pub name: String,
}

impl HelloResponse {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut m = Self::default();
        for (f, v) in fields(buf)? {
            match f {
                1 => m.api_major = v.uint() as u32,
                2 => m.api_minor = v.uint() as u32,
                3 => m.server_info = v.string(),
                4 => m.name = v.string(),
                _ => {}
            }
        }
        Ok(m)
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    pub friendly_name: String,
    pub mac_address: String,
    pub esphome_version: String,
    pub model: String,
    pub manufacturer: String,
    pub project_name: String,
    pub voice_assistant_feature_flags: u32,
}

impl DeviceInfo {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut m = Self::default();
        for (f, v) in fields(buf)? {
            match f {
                2 => m.name = v.string(),
                3 => m.mac_address = v.string(),
                4 => m.esphome_version = v.string(),
                6 => m.model = v.string(),
                8 => m.project_name = v.string(),
                12 => m.manufacturer = v.string(),
                13 => m.friendly_name = v.string(),
                17 => m.voice_assistant_feature_flags = v.uint() as u32,
                _ => {}
            }
        }
        Ok(m)
    }

    pub fn has(&self, feature: u32) -> bool {
        self.voice_assistant_feature_flags & feature != 0
    }
}

pub fn subscribe_voice_assistant(subscribe: bool, flags: u32) -> Vec<u8> {
    Writer::default().bool(1, subscribe).uint(2, flags as u64).finish()
}

/// The device starting (a wake word was heard, or continuous streaming began) or stopping a run.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct VoiceAssistantRequest {
    pub start: bool,
    pub conversation_id: String,
    pub flags: u32,
    pub wake_word_phrase: String,
}

impl VoiceAssistantRequest {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut m = Self::default();
        for (f, v) in fields(buf)? {
            match f {
                1 => m.start = v.uint() != 0,
                2 => m.conversation_id = v.string(),
                3 => m.flags = v.uint() as u32,
                5 => m.wake_word_phrase = v.string(),
                _ => {}
            }
        }
        Ok(m)
    }

    pub fn encode(&self) -> Vec<u8> {
        Writer::default()
            .bool(1, self.start)
            .str(2, &self.conversation_id)
            .uint(3, self.flags as u64)
            .str(5, &self.wake_word_phrase)
            .finish()
    }
}

/// `port` 0: audio comes over the API connection.
pub fn voice_assistant_response(port: u32, error: bool) -> Vec<u8> {
    Writer::default().uint(1, port as u64).bool(2, error).finish()
}

pub fn voice_assistant_event(event: Event, data: &[(&str, &str)]) -> Vec<u8> {
    let mut w = Writer::default();
    w.uint(1, event as u64);
    for (name, value) in data {
        let entry = Writer::default().str(1, name).str(2, value).finish();
        w.message(2, &entry);
    }
    w.finish()
}

/// A timer's state for the device's own display and alarm (`VoiceAssistantTimerEventResponse`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TimerUpdate {
    pub event: u32,
    pub timer_id: String,
    pub name: String,
    pub total_seconds: u32,
    pub seconds_left: u32,
    pub is_active: bool,
}

impl TimerUpdate {
    pub fn encode(&self) -> Vec<u8> {
        Writer::default()
            .uint(1, self.event as u64)
            .str(2, &self.timer_id)
            .str(3, &self.name)
            .uint(4, self.total_seconds as u64)
            .uint(5, self.seconds_left as u64)
            .bool(6, self.is_active)
            .finish()
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut m = Self::default();
        for (f, v) in fields(buf)? {
            match f {
                1 => m.event = v.uint() as u32,
                2 => m.timer_id = v.string(),
                3 => m.name = v.string(),
                4 => m.total_seconds = v.uint() as u32,
                5 => m.seconds_left = v.uint() as u32,
                6 => m.is_active = v.uint() != 0,
                _ => {}
            }
        }
        Ok(m)
    }
}

/// Plays `media_id` (a URL) on the device outside a run (`VoiceAssistantAnnounceRequest`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Announce {
    pub media_id: String,
    pub text: String,
    pub preannounce_media_id: String,
    pub start_conversation: bool,
}

impl Announce {
    pub fn encode(&self) -> Vec<u8> {
        Writer::default()
            .str(1, &self.media_id)
            .str(2, &self.text)
            .str(3, &self.preannounce_media_id)
            .bool(4, self.start_conversation)
            .finish()
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut m = Self::default();
        for (f, v) in fields(buf)? {
            match f {
                1 => m.media_id = v.string(),
                2 => m.text = v.string(),
                3 => m.preannounce_media_id = v.string(),
                4 => m.start_conversation = v.uint() != 0,
                _ => {}
            }
        }
        Ok(m)
    }
}

/// Audio in either direction: 16 kHz mono 16-bit PCM. `data2` is the device's unprocessed
/// channel when it sends two.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct VoiceAssistantAudio {
    pub data: Vec<u8>,
    pub end: bool,
}

impl VoiceAssistantAudio {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut m = Self::default();
        for (f, v) in fields(buf)? {
            match f {
                1 => m.data = v.bytes().to_vec(),
                2 => m.end = v.uint() != 0,
                _ => {}
            }
        }
        Ok(m)
    }

    pub fn encode(&self) -> Vec<u8> {
        Writer::default().bytes(1, &self.data).bool(2, self.end).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for v in [0u64, 1, 127, 128, 300, 16_384, u32::MAX as u64, u64::MAX] {
            assert_eq!(read_varint(&varint(v), 0).unwrap(), (v, varint(v).len()));
        }
    }

    #[test]
    fn messages_round_trip() {
        let req = VoiceAssistantRequest {
            start: true,
            conversation_id: "c1".into(),
            flags: REQUEST_USE_WAKE_WORD,
            wake_word_phrase: "Okay Nabu".into(),
        };
        assert_eq!(VoiceAssistantRequest::decode(&req.encode()).unwrap(), req);
        let audio = VoiceAssistantAudio { data: vec![1, 2, 3, 4], end: false };
        assert_eq!(VoiceAssistantAudio::decode(&audio.encode()).unwrap(), audio);
        let timer = TimerUpdate {
            event: TimerEvent::Finished as u32,
            timer_id: "timer-1".into(),
            name: "pasta".into(),
            total_seconds: 480,
            seconds_left: 0,
            is_active: false,
        };
        assert_eq!(TimerUpdate::decode(&timer.encode()).unwrap(), timer);
        let announce = Announce { media_id: "http://x/a.wav".into(), text: "hi".into(), ..Default::default() };
        assert_eq!(Announce::decode(&announce.encode()).unwrap(), announce);
    }

    #[test]
    fn event_data_is_repeated_name_value_pairs() {
        let buf = voice_assistant_event(Event::SttEnd, &[("text", "hei"), ("x", "")]);
        let f = fields(&buf).unwrap();
        assert_eq!(f[0].1.uint(), Event::SttEnd as u64);
        let entries: Vec<(String, String)> = f[1..]
            .iter()
            .map(|(_, v)| {
                let inner = fields(v.bytes()).unwrap();
                let get = |n| inner.iter().find(|(k, _)| *k == n).map(|(_, v)| v.string()).unwrap_or_default();
                (get(1), get(2))
            })
            .collect();
        assert_eq!(entries, [("text".to_string(), "hei".to_string()), ("x".to_string(), String::new())]);
    }

    #[test]
    fn device_info_reads_the_voice_flags() {
        let buf = Writer::default()
            .str(2, "home-assistant-voice-0a1b2c")
            .str(13, "Home Assistant Voice")
            .uint(17, (feature::VOICE_ASSISTANT | feature::API_AUDIO | feature::ANNOUNCE) as u64)
            .finish();
        let info = DeviceInfo::decode(&buf).unwrap();
        assert_eq!(info.friendly_name, "Home Assistant Voice");
        assert!(info.has(feature::API_AUDIO) && !info.has(feature::SPEAKER));
    }
}
