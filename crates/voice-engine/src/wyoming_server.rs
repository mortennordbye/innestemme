//! A Wyoming server, the protocol of Home Assistant's `wyoming` integration: the engine's listener
//! as speech to text and its voice as text to speech, so an Assist pipeline whose agent is the
//! engine (`custom_components/innestemme`) hears and speaks like the satellite does. Each event is
//! one JSON line, then `data_length` bytes of JSON data and `payload_length` bytes of audio.
//!
//! Wyoming has no authentication: anyone who reaches the port can transcribe and synthesize, which
//! changes nothing in the house.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};
use voice_proto::SAMPLE_RATE;

use crate::assistant::AssistantHandle;

/// The protocol version written in each event header.
const VERSION: &str = "1.11.0";
/// The longest header line, data block and audio chunk taken: more is not a Wyoming client.
const LINE_MAX: usize = 16 * 1024;
const DATA_MAX: usize = 64 * 1024;
const PAYLOAD_MAX: usize = 1 << 20;
/// The longest speech taken for one transcript; Home Assistant stops listening long before.
const AUDIO_MAX_SECONDS: usize = 30;
/// The longest text synthesized: an answer is a few sentences.
const TEXT_MAX: usize = 1000;
/// Weather or departures in the answer being synthesized, on a slow CPU.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// Speech goes back in chunks of 0.1 s.
const CHUNK: usize = SAMPLE_RATE as usize / 10;

/// What Home Assistant lists for this server.
#[derive(Clone)]
pub struct About {
    pub version: String,
    /// The listener, as `--listener` names it.
    pub listener: String,
    /// The voice, as the web page shows it ("Kokoro, am_onyx").
    pub voice: String,
}

/// Serves Wyoming clients until the listener fails.
pub async fn serve(listener: TcpListener, assistant: AssistantHandle, about: About) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                warn!(%error, "wyoming connection not accepted");
                continue;
            }
        };
        let (assistant, about) = (assistant.clone(), about.clone());
        tokio::spawn(async move {
            let (read, write) = stream.into_split();
            if let Err(error) = session(BufReader::new(read), write, &assistant, &about).await {
                debug!(%peer, error = format!("{error:#}"), "wyoming connection closed");
            }
        });
    }
}

/// One client's events until it hangs up.
async fn session<R, W>(mut read: R, mut write: W, assistant: &AssistantHandle, about: &About) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut audio: Option<Audio> = None;
    while let Some(event) = read_event(&mut read).await? {
        match event.kind.as_str() {
            "describe" => write_event(&mut write, "info", &info(about), &[]).await?,
            "ping" => write_event(&mut write, "pong", &event.data, &[]).await?,
            "transcribe" => audio = None,
            "audio-start" => audio = Some(Audio::new(&event.data)?),
            "audio-chunk" => {
                if let Some(audio) = &mut audio {
                    audio.push(&event.payload)?;
                }
            }
            "audio-stop" => {
                let Some(heard) = audio.take() else { continue };
                let pcm = heard.to_engine_rate();
                let seconds = pcm.len() as f32 / SAMPLE_RATE as f32;
                let text = reply(assistant.transcribe(pcm)).await.unwrap_or_default();
                info!(text, seconds, "transcript for Home Assistant");
                write_event(&mut write, "transcript", &json!({ "text": text }), &[]).await?;
            }
            "synthesize" => {
                let text = event.data["text"].as_str().unwrap_or_default().trim().to_owned();
                let pcm = match text.chars().count() {
                    0 => Vec::new(),
                    n if n > TEXT_MAX => {
                        warn!(characters = n, "text for speech over {TEXT_MAX} characters: not spoken");
                        Vec::new()
                    }
                    _ => reply(assistant.speech(text)).await.unwrap_or_default(),
                };
                let format = json!({ "rate": SAMPLE_RATE, "width": 2, "channels": 1 });
                write_event(&mut write, "audio-start", &format, &[]).await?;
                for chunk in pcm.chunks(CHUNK) {
                    let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                    write_event(&mut write, "audio-chunk", &format, &bytes).await?;
                }
                write_event(&mut write, "audio-stop", &json!({}), &[]).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn reply<T>(answer: tokio::sync::oneshot::Receiver<T>) -> Option<T> {
    tokio::time::timeout(REPLY_TIMEOUT, answer).await.ok()?.ok()
}

/// Speech arriving in chunks: 16-bit PCM at the client's rate.
#[derive(Debug)]
struct Audio {
    rate: u32,
    channels: usize,
    bytes: Vec<u8>,
}

impl Audio {
    fn new(format: &Value) -> Result<Self> {
        let number = |key: &str| format[key].as_u64().with_context(|| format!("audio-start without `{key}`"));
        let (rate, width, channels) = (number("rate")?, number("width")?, number("channels")?);
        if width != 2 {
            bail!("only 16-bit audio is transcribed, not {} bytes a sample", width);
        }
        if !(8_000..=48_000).contains(&rate) || !(1..=8).contains(&channels) {
            bail!("audio at {rate} Hz with {channels} channels is not speech");
        }
        Ok(Self { rate: rate as u32, channels: channels as usize, bytes: Vec::new() })
    }

    fn push(&mut self, payload: &[u8]) -> Result<()> {
        let max = self.rate as usize * 2 * self.channels * AUDIO_MAX_SECONDS;
        if self.bytes.len() + payload.len() > max {
            bail!("speech over {AUDIO_MAX_SECONDS} s");
        }
        self.bytes.extend_from_slice(payload);
        Ok(())
    }

    /// The first channel at the engine's rate.
    fn to_engine_rate(&self) -> Vec<i16> {
        let mono: Vec<i16> =
            self.bytes.chunks_exact(2 * self.channels).map(|frame| i16::from_le_bytes([frame[0], frame[1]])).collect();
        voice_assistant::resample::to_24k(&mono, self.rate)
    }
}

/// The `info` Home Assistant asks for with `describe`: one listener and one voice, in English.
fn info(about: &About) -> Value {
    let attribution = json!({ "name": "innestemme", "url": "https://github.com/mortennordbye/innestemme" });
    json!({
        "asr": [{
            "name": "innestemme",
            "description": "innestemme speech to text",
            "attribution": attribution,
            "installed": true,
            "version": about.version,
            "models": [{
                "name": about.listener,
                "description": about.listener,
                "attribution": attribution,
                "installed": true,
                "version": about.version,
                "languages": ["en"],
            }],
            "supports_transcript_streaming": false,
        }],
        "tts": [{
            "name": "innestemme",
            "description": "innestemme text to speech",
            "attribution": attribution,
            "installed": true,
            "version": about.version,
            "voices": [{
                "name": about.voice,
                "description": about.voice,
                "attribution": attribution,
                "installed": true,
                "version": about.version,
                "languages": ["en"],
            }],
            "supports_synthesize_streaming": false,
        }],
    })
}

#[derive(Debug)]
struct Event {
    kind: String,
    data: Value,
    payload: Vec<u8>,
}

/// The next event, or `None` when the client hung up between events.
async fn read_event<R: AsyncBufRead + Unpin>(read: &mut R) -> Result<Option<Event>> {
    let mut line = Vec::new();
    if (&mut *read).take(LINE_MAX as u64 + 1).read_until(b'\n', &mut line).await? == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        bail!("event header over {LINE_MAX} bytes, or cut off");
    }
    let header: Value = serde_json::from_slice(&line).context("event header")?;
    let kind = header["type"].as_str().context("event without a type")?.to_owned();
    let mut data = match header.get("data") {
        Some(Value::Object(inline)) => inline.clone(),
        _ => Default::default(),
    };
    if let Some(n) = length(&header, "data_length", DATA_MAX)? {
        let mut bytes = vec![0; n];
        read.read_exact(&mut bytes).await?;
        if let Value::Object(more) = serde_json::from_slice(&bytes).context("event data")? {
            data.extend(more);
        }
    }
    let mut payload = Vec::new();
    if let Some(n) = length(&header, "payload_length", PAYLOAD_MAX)? {
        payload.resize(n, 0);
        read.read_exact(&mut payload).await?;
    }
    Ok(Some(Event { kind, data: Value::Object(data), payload }))
}

fn length(header: &Value, key: &str, max: usize) -> Result<Option<usize>> {
    match header.get(key).and_then(Value::as_u64) {
        None | Some(0) => Ok(None),
        Some(n) if n as usize > max => bail!("{key} {n} is over {max}"),
        Some(n) => Ok(Some(n as usize)),
    }
}

async fn write_event<W: AsyncWrite + Unpin>(write: &mut W, kind: &str, data: &Value, payload: &[u8]) -> Result<()> {
    let data = match data.as_object() {
        Some(fields) if !fields.is_empty() => serde_json::to_vec(data)?,
        _ => Vec::new(),
    };
    let mut header = json!({ "type": kind, "version": VERSION });
    if !data.is_empty() {
        header["data_length"] = json!(data.len());
    }
    if !payload.is_empty() {
        header["payload_length"] = json!(payload.len());
    }
    let mut out = serde_json::to_vec(&header)?;
    out.push(b'\n');
    out.extend_from_slice(&data);
    out.extend_from_slice(payload);
    write.write_all(&out).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn about() -> About {
        About { version: "0.0.1".into(), listener: "parakeet".into(), voice: "Kokoro, am_onyx".into() }
    }

    async fn events(bytes: &[u8]) -> Vec<Event> {
        let mut read = BufReader::new(bytes);
        let mut out = Vec::new();
        while let Some(event) = read_event(&mut read).await.unwrap() {
            out.push(event);
        }
        out
    }

    async fn written(kind: &str, data: Value, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_event(&mut out, kind, &data, payload).await.unwrap();
        out
    }

    #[tokio::test]
    async fn frames_round_trip_with_data_and_payload() {
        let mut bytes =
            written("audio-chunk", json!({ "rate": 16000, "width": 2, "channels": 1 }), &[1, 0, 2, 0]).await;
        bytes.extend(written("audio-stop", json!({}), &[]).await);
        let header: Value = serde_json::from_slice(bytes.split(|&b| b == b'\n').next().unwrap()).unwrap();
        assert_eq!(header["payload_length"], 4);
        assert!(header.get("data").is_none(), "data goes after the line, as the Python client writes it");
        let read = events(&bytes).await;
        assert_eq!(read.len(), 2);
        assert_eq!(
            (read[0].kind.as_str(), read[0].data["rate"].as_u64(), &read[0].payload[..]),
            ("audio-chunk", Some(16000), &[1u8, 0, 2, 0][..])
        );
        assert_eq!((read[1].kind.as_str(), read[1].payload.len()), ("audio-stop", 0));
    }

    #[tokio::test]
    async fn takes_inline_data_too_and_refuses_oversized_parts() {
        let inline = events(b"{\"type\":\"synthesize\",\"data\":{\"text\":\"Hello.\"}}\n").await;
        assert_eq!(inline[0].data["text"], "Hello.");
        let mut read = BufReader::new(&b"{\"type\":\"audio-chunk\",\"payload_length\":99999999}\n"[..]);
        assert!(read_event(&mut read).await.is_err());
        let long = vec![b'x'; LINE_MAX + 10];
        assert!(read_event(&mut BufReader::new(&long[..])).await.is_err());
    }

    #[test]
    fn audio_takes_the_first_channel_and_caps_its_length() {
        let mut audio = Audio::new(&json!({ "rate": 24000, "width": 2, "channels": 2 })).unwrap();
        audio.push(&[1, 0, 9, 9, 2, 0, 9, 9]).unwrap();
        assert_eq!(audio.to_engine_rate(), [1, 2]);
        assert!(Audio::new(&json!({ "rate": 16000, "width": 4, "channels": 1 })).is_err());
        let mut long = Audio::new(&json!({ "rate": 16000, "width": 2, "channels": 1 })).unwrap();
        assert!(long.push(&vec![0; 16000 * 2 * (AUDIO_MAX_SECONDS + 1)]).is_err());
        // 16 kHz in, the engine's 24 kHz out.
        let mut sixteen = Audio::new(&json!({ "rate": 16000, "width": 2, "channels": 1 })).unwrap();
        sixteen.push(&vec![0; 1600 * 2]).unwrap();
        assert_eq!(sixteen.to_engine_rate().len(), 2400);
    }

    #[tokio::test]
    async fn describes_one_listener_and_one_voice() {
        let (handle, _) = AssistantHandle::answering_for_tests("unused");
        let mut reply = Vec::new();
        session(BufReader::new(&b"{\"type\":\"describe\"}\n"[..]), &mut reply, &handle, &about()).await.unwrap();
        let info = &events(&reply).await[0];
        assert_eq!(info.kind, "info");
        assert_eq!(info.data["asr"][0]["models"][0]["languages"][0], "en");
        assert_eq!(info.data["tts"][0]["voices"][0]["name"], "Kokoro, am_onyx");
        assert_eq!(info.data["tts"][0]["installed"], true);
    }

    #[tokio::test]
    async fn transcribes_speech_and_speaks_text() {
        let (handle, asked) = AssistantHandle::answering_for_tests("Turn off the lights.");
        let mut request = Vec::new();
        request.extend(written("transcribe", json!({ "language": "en" }), &[]).await);
        let format = json!({ "rate": 16000, "width": 2, "channels": 1 });
        request.extend(written("audio-start", format.clone(), &[]).await);
        request.extend(written("audio-chunk", format, &vec![0; 3200]).await);
        request.extend(written("audio-stop", json!({}), &[]).await);
        request.extend(written("synthesize", json!({ "text": "Very good, sir." }), &[]).await);
        let mut reply = Vec::new();
        session(BufReader::new(&request[..]), &mut reply, &handle, &about()).await.unwrap();
        let replies = events(&reply).await;
        let kinds: Vec<&str> = replies.iter().map(|e| e.kind.as_str()).collect();
        // The fake voice's 0.2 s goes back as two 0.1 s chunks.
        assert_eq!(kinds, ["transcript", "audio-start", "audio-chunk", "audio-chunk", "audio-stop"]);
        assert_eq!(replies[0].data["text"], "Turn off the lights.");
        assert_eq!(replies[1].data["rate"], SAMPLE_RATE);
        assert!(!replies[2].payload.is_empty());
        // 0.1 s at 16 kHz reached the listener as 0.1 s at the engine's rate; the text reached the voice.
        assert_eq!(asked(), ["2400 samples", "Very good, sir."]);
    }
}
