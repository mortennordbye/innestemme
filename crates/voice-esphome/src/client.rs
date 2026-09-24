//! One connection to a voice satellite: hello, device info, voice assistant subscription, then a
//! reader task that answers pings and hands voice events to the caller.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use crate::frame::{self, FrameReader, FrameWriter};
use crate::proto::{
    self, feature, id, Announce, DeviceInfo, Event, HelloResponse, TimerUpdate, VoiceAssistantAudio,
    VoiceAssistantRequest,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Home Assistant pings every 20 s; the device drops a client it has not heard from in a while.
const PING_EVERY: Duration = Duration::from_secs(20);
/// The largest audio chunk per message: 1024 samples, 64 ms at 16 kHz.
const AUDIO_CHUNK_BYTES: usize = 2048;

/// What the device sends.
#[derive(Debug)]
pub enum Incoming {
    /// A run starts: the device heard its wake word, or streams continuously for a server-side
    /// wake word (`flags & REQUEST_USE_WAKE_WORD`).
    Start(VoiceAssistantRequest),
    /// The device ended the run (button, timeout).
    Stop,
    /// 16 kHz mono microphone audio.
    Audio(Vec<i16>),
    /// An announcement finished playing.
    AnnounceFinished,
}

#[derive(Clone)]
pub struct Device {
    pub info: DeviceInfo,
    pub hello: HelloResponse,
    /// This end of the connection: an address the device can reach us on.
    pub local_addr: std::net::SocketAddr,
    writer: Arc<Mutex<FrameWriter>>,
}

impl Device {
    /// Connects, identifies the device and subscribes to its voice assistant. Messages arrive on
    /// the receiver until the connection ends (the receiver then closes).
    pub async fn connect(
        address: &str,
        key: Option<&[u8; 32]>,
        client_info: &str,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
            .await
            .with_context(|| format!("connecting to {address} timed out"))?
            .with_context(|| format!("connecting to {address}"))?;
        let local_addr = stream.local_addr()?;
        let (mut reader, mut writer, _noise_name) = frame::open(stream, key).await?;

        writer.write(id::HELLO_REQUEST, &proto::hello_request(client_info)).await?;
        let hello = HelloResponse::decode(&expect(&mut reader, &mut writer, id::HELLO_RESPONSE).await?)?;
        writer.write(id::DEVICE_INFO_REQUEST, &[]).await?;
        let info = DeviceInfo::decode(&expect(&mut reader, &mut writer, id::DEVICE_INFO_RESPONSE).await?)?;
        if !info.has(feature::VOICE_ASSISTANT) {
            bail!("{} ({}) has no voice assistant", info.friendly_name, info.name);
        }
        if !info.has(feature::API_AUDIO) {
            bail!("{} only streams audio over UDP, which is not supported", info.friendly_name);
        }
        writer
            .write(
                id::SUBSCRIBE_VOICE_ASSISTANT_REQUEST,
                &proto::subscribe_voice_assistant(true, proto::SUBSCRIBE_API_AUDIO),
            )
            .await?;

        let writer = Arc::new(Mutex::new(writer));
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(read_loop(reader, writer.clone(), tx.clone(), info.name.clone()));
        tokio::spawn(ping_loop(writer.clone(), tx));
        Ok((Self { info, hello, local_addr, writer }, rx))
    }

    async fn send(&self, kind: u16, payload: &[u8]) -> Result<()> {
        self.writer.lock().await.write(kind, payload).await
    }

    /// Answers a [`Incoming::Start`]: audio over the API connection.
    pub async fn accept_run(&self) -> Result<()> {
        self.send(id::VOICE_ASSISTANT_RESPONSE, &proto::voice_assistant_response(0, false)).await
    }

    /// Refuses a run, e.g. while another one is busy.
    pub async fn refuse_run(&self) -> Result<()> {
        self.send(id::VOICE_ASSISTANT_RESPONSE, &proto::voice_assistant_response(0, true)).await
    }

    pub async fn event(&self, event: Event, data: &[(&str, &str)]) -> Result<()> {
        self.send(id::VOICE_ASSISTANT_EVENT_RESPONSE, &proto::voice_assistant_event(event, data)).await
    }

    /// Streams 16 kHz mono response audio (devices with [`feature::SPEAKER`]).
    pub async fn audio(&self, pcm: &[i16]) -> Result<()> {
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        for chunk in bytes.chunks(AUDIO_CHUNK_BYTES) {
            let msg = VoiceAssistantAudio { data: chunk.to_vec(), end: false };
            self.send(id::VOICE_ASSISTANT_AUDIO, &msg.encode()).await?;
        }
        Ok(())
    }

    /// A timer's state, for devices with [`feature::TIMERS`]: they count down and ring themselves.
    pub async fn timer(&self, update: &TimerUpdate) -> Result<()> {
        self.send(id::VOICE_ASSISTANT_TIMER_EVENT_RESPONSE, &update.encode()).await
    }

    /// Plays a URL outside a run, for devices with [`feature::ANNOUNCE`]; the device answers with
    /// [`Incoming::AnnounceFinished`].
    pub async fn announce(&self, announce: &Announce) -> Result<()> {
        self.send(id::VOICE_ASSISTANT_ANNOUNCE_REQUEST, &announce.encode()).await
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.send(id::DISCONNECT_REQUEST, &[]).await
    }
}

/// Reads until `wanted` arrives, answering pings on the way.
async fn expect(reader: &mut FrameReader, writer: &mut FrameWriter, wanted: u16) -> Result<Vec<u8>> {
    loop {
        let (kind, payload) = reader.read().await?;
        match kind {
            k if k == wanted => return Ok(payload),
            id::PING_REQUEST => writer.write(id::PING_RESPONSE, &[]).await?,
            id::DISCONNECT_REQUEST => bail!("the device closed the connection"),
            other => debug!(other, "ignored during connect"),
        }
    }
}

async fn read_loop(mut reader: FrameReader, writer: Arc<Mutex<FrameWriter>>, tx: mpsc::Sender<Incoming>, name: String) {
    let result: Result<()> = async {
        loop {
            let (kind, payload) = reader.read().await?;
            let incoming = match kind {
                id::PING_REQUEST => {
                    writer.lock().await.write(id::PING_RESPONSE, &[]).await?;
                    continue;
                }
                id::DISCONNECT_REQUEST => {
                    let _ = writer.lock().await.write(id::DISCONNECT_RESPONSE, &[]).await;
                    return Ok(());
                }
                id::VOICE_ASSISTANT_REQUEST => {
                    let request = VoiceAssistantRequest::decode(&payload)?;
                    if request.start {
                        Incoming::Start(request)
                    } else {
                        Incoming::Stop
                    }
                }
                id::VOICE_ASSISTANT_AUDIO => {
                    let audio = VoiceAssistantAudio::decode(&payload)?;
                    let (samples, _) = audio.data.as_chunks::<2>();
                    Incoming::Audio(samples.iter().map(|b| i16::from_le_bytes(*b)).collect())
                }
                id::VOICE_ASSISTANT_ANNOUNCE_FINISHED => Incoming::AnnounceFinished,
                id::PING_RESPONSE => continue,
                other => {
                    debug!(other, "ignored message");
                    continue;
                }
            };
            if tx.send(incoming).await.is_err() {
                return Ok(());
            }
        }
    }
    .await;
    if let Err(error) = result {
        warn!(device = name, %error, "satellite connection ended");
    }
}

async fn ping_loop(writer: Arc<Mutex<FrameWriter>>, tx: mpsc::Sender<Incoming>) {
    let mut every = tokio::time::interval(PING_EVERY);
    every.tick().await;
    loop {
        every.tick().await;
        if tx.is_closed() || writer.lock().await.write(id::PING_REQUEST, &[]).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;
    use crate::proto::Writer;

    /// A fake voice satellite: answers hello and device info, then starts a run and streams audio.
    async fn fake_device(listener: TcpListener, key: Option<[u8; 32]>) -> Result<(u16, Vec<u8>)> {
        let (stream, _) = listener.accept().await?;
        let (mut r, mut w) = match key {
            None => {
                let (r, w, _) = frame::open(stream, None).await?;
                (r, w)
            }
            Some(key) => responder(stream, &key).await?,
        };
        let mut subscribed = false;
        while !subscribed {
            let (kind, _) = r.read().await?;
            match kind {
                id::HELLO_REQUEST => {
                    let hello = Writer::default().uint(1, 1).uint(2, 12).str(3, "fake").str(4, "voice-pe").finish();
                    w.write(id::HELLO_RESPONSE, &hello).await?
                }
                id::DEVICE_INFO_REQUEST => {
                    let flags = feature::VOICE_ASSISTANT | feature::API_AUDIO | feature::SPEAKER;
                    let info = Writer::default().str(2, "voice-pe").str(13, "Voice PE").uint(17, flags as u64).finish();
                    w.write(id::DEVICE_INFO_RESPONSE, &info).await?
                }
                id::SUBSCRIBE_VOICE_ASSISTANT_REQUEST => subscribed = true,
                _ => {}
            }
        }
        w.write(id::PING_REQUEST, &[]).await?;
        let start = VoiceAssistantRequest { start: true, flags: proto::REQUEST_USE_WAKE_WORD, ..Default::default() };
        w.write(id::VOICE_ASSISTANT_REQUEST, &start.encode()).await?;
        let audio = VoiceAssistantAudio { data: vec![1, 0, 0xff, 0xff], end: false };
        w.write(id::VOICE_ASSISTANT_AUDIO, &audio.encode()).await?;
        // The client answers the ping and accepts the run.
        let mut seen = Vec::new();
        loop {
            let (kind, payload) = r.read().await?;
            seen.push(kind);
            if kind == id::VOICE_ASSISTANT_RESPONSE {
                assert!(seen.contains(&id::PING_RESPONSE), "ping answered before the run: {seen:?}");
                return Ok((kind, payload));
            }
        }
    }

    /// The device side of the Noise handshake.
    async fn responder(stream: TcpStream, key: &[u8; 32]) -> Result<(FrameReader, FrameWriter)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = stream;
        let mut hs = snow::Builder::new(frame::NOISE_PATTERN.parse()?)
            .prologue(frame::NOISE_PROLOGUE)?
            .psk(0, key)?
            .build_responder()?;
        let mut hello = [0u8; 3];
        stream.read_exact(&mut hello).await?;
        assert_eq!(hello, [1, 0, 0]);
        let mut header = [0u8; 3];
        stream.read_exact(&mut header).await?;
        let mut frame_buf = vec![0u8; u16::from_be_bytes([header[1], header[2]]) as usize];
        stream.read_exact(&mut frame_buf).await?;
        assert_eq!(frame_buf[0], 0);
        let mut buf = vec![0u8; 1024];
        hs.read_message(&frame_buf[1..], &mut buf)?;
        let server_hello = b"\x01voice-pe\x00aa:bb\x00";
        let mut out = vec![1u8];
        out.extend((server_hello.len() as u16).to_be_bytes());
        out.extend_from_slice(server_hello);
        let n = hs.write_message(&[], &mut buf)?;
        out.push(1);
        out.extend(((n + 1) as u16).to_be_bytes());
        out.push(0);
        out.extend_from_slice(&buf[..n]);
        stream.write_all(&out).await?;
        let transport = Arc::new(hs.into_stateless_transport_mode()?);
        // Reuse the client's framing with the directions swapped: same message layout.
        Ok(frame::from_parts(stream, transport))
    }

    async fn run(key: Option<[u8; 32]>) {
        tokio::time::timeout(Duration::from_secs(10), session(key)).await.expect("session hung");
    }

    async fn session(key: Option<[u8; 32]>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let device = tokio::spawn(fake_device(listener, key));
        let (dev, mut rx) = Device::connect(&address, key.as_ref(), "test").await.unwrap();
        assert_eq!(dev.info.friendly_name, "Voice PE");
        assert_eq!(dev.hello.name, "voice-pe");
        let Some(Incoming::Start(start)) = rx.recv().await else { panic!("no start") };
        assert_eq!(start.flags, proto::REQUEST_USE_WAKE_WORD);
        let Some(Incoming::Audio(pcm)) = rx.recv().await else { panic!("no audio") };
        assert_eq!(pcm, [1, -1]);
        dev.accept_run().await.unwrap();
        let (kind, payload) = device.await.unwrap().unwrap();
        assert_eq!(kind, id::VOICE_ASSISTANT_RESPONSE);
        assert!(proto::fields(&payload).unwrap().is_empty(), "port 0 and no error encode as empty");
    }

    #[tokio::test]
    async fn plaintext_session() {
        run(None).await;
    }

    #[tokio::test]
    async fn encrypted_session() {
        let key = frame::parse_key("px7tsbK3C7bpXHr2OevEV2ZMg/FsNTw2dH1uaA2Z3ts=").unwrap();
        run(Some(key)).await;
    }
}
