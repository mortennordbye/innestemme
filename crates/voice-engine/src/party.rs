//! Party mode: every Hue light in the apartment follows the music. The beat comes from the
//! satellite's microphone, which streams all the time for the wake word, so it follows whatever
//! plays in the room. The lights get it through the Hue Entertainment API: one DTLS stream to the
//! bridge, 50 frames a second, which the bridge passes on to the lights about 25 times a second.
//! Commands through Home Assistant cannot keep up: Hue takes about one room command a second.
//!
//! Two modes: normal moves a new colour round the room on each beat and fades between them; fast
//! flashes on every hit, as fast as the lights follow.

use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tracing::{info, warn};

/// The microphone's rate.
const RATE: f32 = 16_000.0;
/// Analysis step: 10 ms.
const HOP: usize = 160;
const FRAME: Duration = Duration::from_millis(20);
/// The entertainment area innestemme makes on the bridge, with every light that can stream.
const AREA_NAME: &str = "innestemme party";
/// The bridge's entertainment stream (DTLS over UDP).
const STREAM_PORT: u16 = 2100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Fast,
}

impl Mode {
    /// Shortest gap between beats: a normal beat at most 4 times a second, a fast hit as often
    /// as the lights can show a flash and its dark after (about 25 updates a second).
    fn min_gap(self) -> Duration {
        match self {
            Mode::Normal => Duration::from_millis(250),
            Mode::Fast => Duration::from_millis(80),
        }
    }

    /// How far above the usual change in loudness a change must be to count as a beat.
    fn sensitivity(self) -> f32 {
        match self {
            Mode::Normal => 1.6,
            Mode::Fast => 0.9,
        }
    }
}

/// The bridge and the keys it gave innestemme (`generateclientkey` after the link button).
#[derive(Clone, Debug)]
pub struct HueConfig {
    pub bridge: String,
    pub app_key: String,
    /// 32 hex digits: the DTLS pre-shared key.
    pub client_key: String,
}

/// Shared by the satellite, which feeds it the microphone, and the assistant, which starts and
/// stops it.
#[derive(Clone)]
pub struct Party {
    inner: Arc<Inner>,
}

struct Inner {
    hue: Option<HueConfig>,
    /// 0 off, else the mode the stream runs in.
    mode: AtomicU8,
    beats: Mutex<Beats>,
}

impl Party {
    pub fn new(hue: Option<HueConfig>) -> Self {
        Self { inner: Arc::new(Inner { hue, mode: AtomicU8::new(0), beats: Mutex::new(Beats::default()) }) }
    }

    pub fn configured(&self) -> bool {
        self.inner.hue.is_some()
    }

    pub fn mode(&self) -> Option<Mode> {
        match self.inner.mode.load(Relaxed) {
            1 => Some(Mode::Normal),
            2 => Some(Mode::Fast),
            _ => None,
        }
    }

    /// Starts the stream, or changes the mode of the one running.
    pub fn start(&self, mode: Mode) -> Result<()> {
        let Some(hue) = self.inner.hue.clone() else { bail!("no Hue bridge configured") };
        let was = self.inner.mode.swap(if mode == Mode::Normal { 1 } else { 2 }, Relaxed);
        if was != 0 {
            info!(?mode, "party mode changed");
            return Ok(());
        }
        info!(?mode, "party mode on");
        let party = self.clone();
        std::thread::Builder::new().name("party".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => return warn!(%error, "party mode: no runtime"),
            };
            if let Err(error) = runtime.block_on(party.stream(&hue)) {
                warn!(error = format!("{error:#}"), "party mode stopped");
            }
            party.inner.mode.store(0, Relaxed);
        })?;
        Ok(())
    }

    pub fn stop(&self) {
        if self.inner.mode.swap(0, Relaxed) != 0 {
            info!("party mode off");
        }
    }

    /// The satellite's microphone, 16 kHz.
    pub fn feed(&self, pcm: &[i16]) {
        let Some(mode) = self.mode() else { return };
        if let Ok(mut beats) = self.inner.beats.lock() {
            beats.push(pcm, mode);
        }
    }

    async fn stream(&self, hue: &HueConfig) -> Result<()> {
        let bridge = Bridge::new(hue)?;
        let (area, channels) = {
            let bridge = bridge.clone();
            tokio::task::spawn_blocking(move || bridge.area()).await??
        };
        info!(area, channels = channels.len(), "party mode: entertainment area");
        {
            let (bridge, area) = (bridge.clone(), area.clone());
            tokio::task::spawn_blocking(move || bridge.action(&area, "start")).await??;
        }
        let result = self.send_frames(hue, &area, &channels).await;
        let stopped = tokio::task::spawn_blocking(move || bridge.action(&area, "stop")).await;
        if let Ok(Err(error)) | Err(error) = stopped.map_err(anyhow::Error::from) {
            warn!(error = format!("{error:#}"), "party mode: could not stop the entertainment area");
        }
        result
    }

    async fn send_frames(&self, hue: &HueConfig, area: &str, channels: &[u8]) -> Result<()> {
        let dtls = connect(hue).await?;
        let mut show = Show::new(channels.len());
        let mut sequence = 0u8;
        let mut tick = tokio::time::interval(FRAME);
        let start = Instant::now();
        let (mut counted, mut since) = (0u32, Instant::now());
        while let Some(mode) = self.mode() {
            tick.tick().await;
            let (beats, level) = match self.inner.beats.lock() {
                Ok(mut b) => (std::mem::take(&mut b.new_beats), b.level),
                Err(_) => (0, 0.0),
            };
            counted += beats;
            if since.elapsed() >= Duration::from_secs(10) {
                info!(?mode, beats = counted, level = format!("{level:.2}"), "party mode: beats in 10 s");
                (counted, since) = (0, Instant::now());
            }
            let colours = show.frame(mode, beats, level, start.elapsed());
            let message = message(sequence, area, channels, &colours);
            sequence = sequence.wrapping_add(1);
            dtls.write(&message, None).await.context("sending to the bridge")?;
        }
        let _ = dtls.close().await;
        Ok(())
    }
}

/// Beats in the microphone: a jump in low-frequency loudness well above the usual jumps.
#[derive(Default)]
struct Beats {
    pending: Vec<i16>,
    /// Low-passed signal, for the kick and bass.
    low: f32,
    last_energy: f32,
    /// Running mean and spread of the jumps.
    mean: f32,
    spread: f32,
    since_beat: Duration,
    /// Beats since the renderer last looked.
    new_beats: u32,
    /// Overall loudness, 0 to 1 against the loudest lately.
    level: f32,
    peak: f32,
}

impl Beats {
    fn push(&mut self, pcm: &[i16], mode: Mode) {
        self.pending.extend_from_slice(pcm);
        let hops = self.pending.len() / HOP;
        for i in 0..hops {
            let hop: Vec<f32> = self.pending[i * HOP..(i + 1) * HOP].iter().map(|&s| s as f32 / 32768.0).collect();
            self.hop(&hop, mode);
        }
        self.pending.drain(..hops * HOP);
    }

    fn hop(&mut self, hop: &[f32], mode: Mode) {
        // One pole at about 150 Hz.
        let alpha = 1.0 - (-2.0 * std::f32::consts::PI * 150.0 / RATE).exp();
        let (mut low_energy, mut energy) = (0.0f32, 0.0f32);
        for &s in hop {
            self.low += alpha * (s - self.low);
            low_energy += self.low * self.low;
            energy += s * s;
        }
        let low_energy = (low_energy / hop.len() as f32).sqrt();
        let rms = (energy / hop.len() as f32).sqrt();
        self.peak = (self.peak * 0.9995).max(rms).max(1e-4);
        self.level = (rms / self.peak).min(1.0);

        let jump = (low_energy - self.last_energy).max(0.0);
        self.last_energy = low_energy;
        self.since_beat += Duration::from_millis(10);
        // About 1.5 s of memory.
        let k = 0.007;
        let deviation = (jump - self.mean).abs();
        let is_beat =
            jump > self.mean + mode.sensitivity() * self.spread && jump > 1e-3 && self.since_beat >= mode.min_gap();
        self.mean += k * (jump - self.mean);
        self.spread += k * (deviation - self.spread);
        if is_beat {
            self.since_beat = Duration::ZERO;
            self.new_beats += 1;
        }
    }
}

/// What the lights show, frame by frame.
struct Show {
    lights: usize,
    hue: f32,
    /// Brightness envelope, 1 on a beat.
    flash: f32,
    beat: u32,
    last: Duration,
}

impl Show {
    fn new(lights: usize) -> Self {
        Self { lights, hue: 0.0, flash: 0.0, beat: 0, last: Duration::ZERO }
    }

    /// RGB per light, 0 to 1.
    fn frame(&mut self, mode: Mode, beats: u32, level: f32, now: Duration) -> Vec<[f32; 3]> {
        let dt = (now - self.last).as_secs_f32();
        self.last = now;
        // Dark again within two of the bridge's updates in fast mode, so the next hit is a new flash.
        let fade = if mode == Mode::Fast { 0.03 } else { 0.35 };
        self.flash *= (-dt / fade).exp();
        if beats > 0 {
            self.beat = self.beat.wrapping_add(beats);
            self.flash = 1.0;
            // A step round the colour wheel that does not come back soon.
            self.hue = (self.hue + 0.382) % 1.0;
        }
        match mode {
            Mode::Normal => {
                let brightness = (0.25 + 0.25 * level).max(self.flash);
                (0..self.lights)
                    .map(|i| hsv((self.hue + i as f32 / self.lights as f32 * 0.3) % 1.0, 1.0, brightness))
                    .collect()
            }
            Mode::Fast => {
                // Half the lights on one hit, the other half on the next; every fourth hit white.
                let white = self.beat.is_multiple_of(4);
                (0..self.lights)
                    .map(|i| {
                        let lit = self.beat % 2 == (i % 2) as u32 || white;
                        let v = if lit { self.flash } else { 0.0 };
                        hsv(self.hue, if white { 0.0 } else { 1.0 }, v)
                    })
                    .collect()
            }
        }
    }
}

fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s));
    match i as i32 % 6 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// A HueStream v2 message: header, the area's id, then a channel id and 16-bit RGB per light.
fn message(sequence: u8, area: &str, channels: &[u8], colours: &[[f32; 3]]) -> Vec<u8> {
    let mut m = Vec::with_capacity(52 + 7 * channels.len());
    m.extend_from_slice(b"HueStream");
    m.extend_from_slice(&[2, 0, sequence, 0, 0, 0, 0]);
    m.extend_from_slice(area.as_bytes());
    for (channel, rgb) in channels.iter().zip(colours) {
        m.push(*channel);
        for c in rgb {
            m.extend_from_slice(&((c.clamp(0.0, 1.0) * 65535.0) as u16).to_be_bytes());
        }
    }
    m
}

async fn connect(hue: &HueConfig) -> Result<webrtc_dtls::conn::DTLSConn> {
    use webrtc_dtls::cipher_suite::CipherSuiteId;
    use webrtc_dtls::config::{Config, ExtendedMasterSecretType};
    let key = decode_hex(&hue.client_key).context("the Hue client key is not hex")?;
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect((hue.bridge.as_str(), STREAM_PORT)).await?;
    let config = Config {
        psk: Some(Arc::new(move |_hint: &[u8]| Ok(key.clone()))),
        // Sent by a client as its identity: the bridge looks the key up by the app key.
        psk_identity_hint: Some(hue.app_key.as_bytes().to_vec()),
        cipher_suites: vec![CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256],
        extended_master_secret: ExtendedMasterSecretType::Disable,
        ..Default::default()
    };
    let connect = webrtc_dtls::conn::DTLSConn::new(Arc::new(socket), config, true, None);
    tokio::time::timeout(Duration::from_secs(5), connect)
        .await
        .context("no DTLS handshake with the bridge in 5 s")?
        .context("DTLS handshake with the bridge")
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        bail!("odd length");
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(Into::into)).collect()
}

/// The bridge's CLIP v2 API, for the entertainment area.
#[derive(Clone)]
struct Bridge {
    agent: ureq::Agent,
    base: String,
    key: String,
}

impl Bridge {
    fn new(hue: &HueConfig) -> Result<Self> {
        // The bridge's certificate is signed by Signify's own root, for its id rather than its address.
        let tls = ureq::tls::TlsConfig::builder().disable_verification(true).build();
        let agent =
            ureq::Agent::config_builder().tls_config(tls).timeout_global(Some(Duration::from_secs(5))).build().into();
        Ok(Self { agent, base: format!("https://{}/clip/v2/resource", hue.bridge), key: hue.app_key.clone() })
    }

    fn get(&self, path: &str) -> Result<Vec<Value>> {
        let mut response =
            self.agent.get(format!("{}/{path}", self.base)).header("hue-application-key", &self.key).call()?;
        let body: Value = response.body_mut().read_json()?;
        Ok(body["data"].as_array().cloned().unwrap_or_default())
    }

    /// The area's id and its channel ids, made with every light that can stream if it is not there.
    fn area(&self) -> Result<(String, Vec<u8>)> {
        let find = |areas: Vec<Value>| areas.into_iter().find(|a| a["metadata"]["name"] == AREA_NAME);
        let area = match find(self.get("entertainment_configuration")?) {
            Some(area) => area,
            None => {
                let renderers: Vec<Value> =
                    self.get("entertainment")?.into_iter().filter(|e| e["renderer"] == true).collect();
                if renderers.is_empty() {
                    bail!("no Hue lights that can stream");
                }
                // Positions matter only to effects that use them; these are spread on a circle.
                let locations: Vec<Value> = renderers
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let angle = i as f64 / renderers.len() as f64 * std::f64::consts::TAU;
                        json!({
                            "service": { "rid": e["id"], "rtype": "entertainment" },
                            "positions": [{ "x": angle.cos(), "y": angle.sin(), "z": 0.0 }],
                        })
                    })
                    .collect();
                let body = json!({
                    "type": "entertainment_configuration",
                    "metadata": { "name": AREA_NAME },
                    "configuration_type": "music",
                    "locations": { "service_locations": locations },
                });
                self.agent
                    .post(format!("{}/entertainment_configuration", self.base))
                    .header("hue-application-key", &self.key)
                    .send_json(body)
                    .context("making the entertainment area")?;
                info!(lights = renderers.len(), "party mode: made the entertainment area");
                find(self.get("entertainment_configuration")?).context("the new entertainment area is missing")?
            }
        };
        let id = area["id"].as_str().context("entertainment area without an id")?.to_owned();
        let channels: Vec<u8> = area["channels"]
            .as_array()
            .map(|c| c.iter().filter_map(|c| c["channel_id"].as_u64()).map(|c| c as u8).collect())
            .unwrap_or_default();
        Ok((id, channels))
    }

    fn action(&self, area: &str, action: &str) -> Result<()> {
        self.agent
            .put(format!("{}/entertainment_configuration/{area}", self.base))
            .header("hue-application-key", &self.key)
            .send_json(json!({ "action": action }))
            .with_context(|| format!("entertainment area {action}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kick drum: a burst of 60 Hz every `period`, with quiet noise between.
    fn kicks(period: Duration, seconds: f32) -> Vec<i16> {
        let n = (seconds * RATE) as usize;
        let every = (period.as_secs_f32() * RATE) as usize;
        (0..n)
            .map(|i| {
                let t = i % every;
                let kick = if t < 1600 {
                    (1.0 - t as f32 / 1600.0) * (i as f32 * 60.0 * std::f32::consts::TAU / RATE).sin()
                } else {
                    0.0
                };
                let noise = ((i * 7919) % 13) as f32 / 13.0 - 0.5;
                ((0.6 * kick + 0.01 * noise) * 32767.0) as i16
            })
            .collect()
    }

    #[test]
    fn finds_the_beat() {
        // 120 beats a minute for 8 s: about 16 beats once it has settled.
        let mut beats = Beats::default();
        for chunk in kicks(Duration::from_millis(500), 8.0).chunks(320) {
            beats.push(chunk, Mode::Normal);
        }
        assert!((12..=18).contains(&beats.new_beats), "{}", beats.new_beats);
    }

    #[test]
    fn fast_mode_follows_fast_hits() {
        // 16th notes at 150 bpm: a hit every 100 ms.
        let mut beats = Beats::default();
        for chunk in kicks(Duration::from_millis(100), 4.0).chunks(320) {
            beats.push(chunk, Mode::Fast);
        }
        assert!(beats.new_beats >= 25, "{}", beats.new_beats);
    }

    #[test]
    fn fast_flashes_go_dark_between_hits() {
        let mut show = Show::new(4);
        let lit = show.frame(Mode::Fast, 1, 0.5, Duration::from_millis(20));
        assert!(lit.iter().any(|c| c.iter().any(|v| *v > 0.9)));
        let dark = show.frame(Mode::Fast, 0, 0.5, Duration::from_millis(100));
        assert!(dark.iter().all(|c| c.iter().all(|v| *v < 0.1)), "{dark:?}");
    }

    #[test]
    fn messages_are_hue_stream_v2() {
        let area = "1a8d99cc-967b-44f2-9202-43f976c0fa6b";
        let m = message(7, area, &[0, 1], &[[1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]);
        assert_eq!(&m[..9], b"HueStream");
        assert_eq!(&m[9..16], &[2, 0, 7, 0, 0, 0, 0]);
        assert_eq!(&m[16..52], area.as_bytes());
        assert_eq!(&m[52..59], &[0, 0xff, 0xff, 0, 0, 0, 0]);
        assert_eq!(m.len(), 52 + 14);
        assert_eq!(decode_hex("0aFF").unwrap(), [10, 255]);
    }
}
