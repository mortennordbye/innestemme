//! The Wyoming protocol (Rhasspy / Home Assistant voice): events over TCP, each a JSON header line
//! followed by optional JSON data and a binary payload. Used for Piper speech synthesis, whether that
//! is Home Assistant's Piper add-on, a sidecar container or a local `rhasspy/wyoming-piper`.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::lang::Lang;
use crate::tts::Tts;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// A long sentence on a slow CPU; Piper sends a sentence's audio once it is synthesized.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
const VERSION: &str = "1.5.4";

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: String,
    pub data: Map<String, Value>,
    pub payload: Vec<u8>,
}

impl Event {
    pub fn new(kind: &str, data: Value) -> Self {
        let data = match data {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        Self { kind: kind.to_owned(), data, payload: Vec::new() }
    }
}

pub fn write_event(w: &mut impl Write, event: &Event) -> Result<()> {
    let data = serde_json::to_vec(&event.data)?;
    let mut header = json!({ "type": event.kind, "version": VERSION });
    if !event.data.is_empty() {
        header["data_length"] = data.len().into();
    }
    if !event.payload.is_empty() {
        header["payload_length"] = event.payload.len().into();
    }
    let mut line = serde_json::to_vec(&header)?;
    line.push(b'\n');
    w.write_all(&line)?;
    if !event.data.is_empty() {
        w.write_all(&data)?;
    }
    w.write_all(&event.payload)?;
    w.flush()?;
    Ok(())
}

/// `Ok(None)` at a clean end of stream.
pub fn read_event(r: &mut impl BufRead) -> Result<Option<Event>> {
    let mut line = Vec::new();
    if r.read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    let header: Value = serde_json::from_slice(&line).context("Wyoming header is not JSON")?;
    let kind = header["type"].as_str().ok_or_else(|| anyhow!("Wyoming header without a type"))?.to_owned();
    // Data may be inline in the header (older peers) and/or follow it.
    let mut data = header["data"].as_object().cloned().unwrap_or_default();
    if let Some(n) = header["data_length"].as_u64().filter(|&n| n > 0) {
        let mut buf = vec![0; n as usize];
        r.read_exact(&mut buf)?;
        let extra: Value = serde_json::from_slice(&buf).context("Wyoming data is not JSON")?;
        data.extend(extra.as_object().cloned().unwrap_or_default());
    }
    let mut payload = Vec::new();
    if let Some(n) = header["payload_length"].as_u64().filter(|&n| n > 0) {
        payload.resize(n as usize, 0);
        r.read_exact(&mut payload)?;
    }
    Ok(Some(Event { kind, data, payload }))
}

/// Piper (or any Wyoming TTS server) at `address` ("host:port"), one voice per language.
pub struct WyomingTts {
    pub address: String,
    pub english_voice: Option<String>,
    pub norwegian_voice: Option<String>,
}

impl WyomingTts {
    fn connect(&self) -> Result<TcpStream> {
        let addr = self
            .address
            .to_socket_addrs()
            .with_context(|| format!("resolving Wyoming TTS {}", self.address))?
            .next()
            .ok_or_else(|| anyhow!("Wyoming TTS {} has no address", self.address))?;
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .with_context(|| format!("connecting to Wyoming TTS {}", self.address))?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    /// The server's description (`info` event data), for a start-up check.
    pub fn describe(&self) -> Result<Map<String, Value>> {
        let mut stream = self.connect()?;
        write_event(&mut stream, &Event::new("describe", Value::Null))?;
        let mut reader = BufReader::new(stream);
        loop {
            match read_event(&mut reader)? {
                Some(e) if e.kind == "info" => return Ok(e.data),
                Some(_) => continue,
                None => bail!("Wyoming TTS {} closed without describing itself", self.address),
            }
        }
    }

    /// Voice names the server offers, and whether each is installed.
    pub fn voices(&self) -> Result<Vec<(String, bool)>> {
        let info = self.describe()?;
        let programs = info.get("tts").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(programs
            .iter()
            .flat_map(|p| p["voices"].as_array().cloned().unwrap_or_default())
            .filter_map(|v| Some((v["name"].as_str()?.to_owned(), v["installed"].as_bool().unwrap_or(true))))
            .collect())
    }
}

impl Tts for WyomingTts {
    fn speak(&mut self, text: &str, lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        let voice = match lang {
            Lang::English => &self.english_voice,
            Lang::Norwegian => &self.norwegian_voice,
        };
        let mut data = json!({ "text": text });
        if let Some(voice) = voice {
            data["voice"] = json!({ "name": voice });
        }
        let mut stream = self.connect()?;
        write_event(&mut stream, &Event::new("synthesize", data))?;
        let mut reader = BufReader::new(stream);
        let (mut rate, mut pcm) = (0u32, Vec::new());
        loop {
            let Some(event) = read_event(&mut reader)? else { bail!("Wyoming TTS closed before audio-stop") };
            match event.kind.as_str() {
                "audio-start" | "audio-chunk" => {
                    rate = event.data.get("rate").and_then(Value::as_u64).map_or(rate, |r| r as u32);
                    let width = event.data.get("width").and_then(Value::as_u64).unwrap_or(2);
                    let channels = event.data.get("channels").and_then(Value::as_u64).unwrap_or(1);
                    if width != 2 || channels != 1 {
                        bail!("Wyoming TTS sent {width}-byte {channels}-channel audio; expected 16-bit mono");
                    }
                    let (samples, _) = event.payload.as_chunks::<2>();
                    pcm.extend(samples.iter().map(|b| i16::from_le_bytes(*b)));
                }
                "audio-stop" => break,
                "error" => bail!("Wyoming TTS: {}", event.data.get("text").and_then(Value::as_str).unwrap_or("error")),
                _ => {}
            }
        }
        if rate == 0 {
            bail!("Wyoming TTS sent no audio format");
        }
        sink(&crate::resample::to_24k(&pcm, rate));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip() {
        let mut event = Event::new("audio-chunk", json!({ "rate": 22050, "width": 2, "channels": 1 }));
        event.payload = vec![1, 0, 255, 255];
        let mut wire = Vec::new();
        write_event(&mut wire, &event).unwrap();
        write_event(&mut wire, &Event::new("audio-stop", Value::Null)).unwrap();
        let mut reader = std::io::Cursor::new(wire);
        assert_eq!(read_event(&mut reader).unwrap(), Some(event));
        assert_eq!(read_event(&mut reader).unwrap().unwrap().kind, "audio-stop");
        assert_eq!(read_event(&mut reader).unwrap(), None);
    }

    #[test]
    fn inline_data_from_older_peers_is_read() {
        let wire = b"{\"type\": \"info\", \"data\": {\"a\": 1}}\n".to_vec();
        let event = read_event(&mut std::io::Cursor::new(wire)).unwrap().unwrap();
        assert_eq!(event.data["a"], 1);
    }
}
