//! Kokoro speech through Kokoro-FastAPI's OpenAI-style endpoint (`/v1/audio/speech`), English only.
//! Raw PCM is asked for, streamed: Kokoro speaks at 24 kHz, the engine's own rate, so audio goes to
//! the sink as it arrives with no decoding or resampling.

use std::io::Read;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::lang::Lang;
use crate::tts::Tts;

/// A long answer on a slow CPU still finishes well inside this.
const TIMEOUT: Duration = Duration::from_secs(30);
/// 100 ms of audio per read.
const CHUNK_BYTES: usize = 4800;

pub struct KokoroTts {
    url: String,
    voice: String,
    agent: ureq::Agent,
}

impl KokoroTts {
    /// `url` is the server's base, e.g. http://127.0.0.1:8880.
    pub fn new(url: &str, voice: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_owned(),
            voice: voice.to_owned(),
            agent: ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into(),
        }
    }

    /// The voices the server offers, for a start-up check.
    pub fn voices(&self) -> Result<Vec<String>> {
        let url = format!("{}/v1/audio/voices", self.url);
        let reply: serde_json::Value =
            self.agent.get(&url).call().with_context(|| format!("asking Kokoro at {url}"))?.body_mut().read_json()?;
        Ok(reply["voices"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect())
    }

    pub fn voice(&self) -> &str {
        &self.voice
    }
}

impl Tts for KokoroTts {
    fn speak(&mut self, text: &str, _lang: Lang, sink: &mut dyn FnMut(&[i16])) -> Result<()> {
        let body = json!({
            "model": "kokoro",
            "input": text,
            "voice": self.voice,
            "response_format": "pcm",
            "stream": true,
        });
        let response =
            self.agent.post(format!("{}/v1/audio/speech", self.url)).send_json(body).context("Kokoro speech")?;
        let mut reader = response.into_body().into_reader();
        let (mut buf, mut held) = (vec![0u8; CHUNK_BYTES + 1], 0usize);
        let mut any = false;
        loop {
            let n = reader.read(&mut buf[held..]).context("reading Kokoro audio")?;
            if n == 0 {
                break;
            }
            let have = held + n;
            // A read can end in the middle of a sample; keep that byte for the next one.
            let whole = have & !1;
            let (samples, _) = buf[..whole].as_chunks::<2>();
            let pcm: Vec<i16> = samples.iter().map(|b| i16::from_le_bytes(*b)).collect();
            if !pcm.is_empty() {
                any = true;
                sink(&pcm);
            }
            held = have - whole;
            if held == 1 {
                buf[0] = buf[whole];
            }
        }
        if !any {
            bail!("Kokoro sent no audio");
        }
        Ok(())
    }
}
