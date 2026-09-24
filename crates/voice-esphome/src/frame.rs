//! ESPHome native API framing over TCP, plaintext or Noise-encrypted.
//!
//! Plaintext: `0x00`, varint payload length, varint message type, payload.
//! Noise (`Noise_NNpsk0_25519_ChaChaPoly_SHA256`, prologue `NoiseAPIInit\0\0`, the device's
//! base64 API encryption key as PSK): `0x01`, u16 big-endian frame length, frame. After the
//! handshake each frame decrypts to u16 type, u16 length, payload (big-endian).

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use snow::StatelessTransportState;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::proto::{read_varint, varint};

pub const NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
pub const NOISE_PROLOGUE: &[u8] = b"NoiseAPIInit\x00\x00";
/// Noise's maximum message size.
const MAX_FRAME: usize = 65_535;
const TAG: usize = 16;

pub struct FrameReader {
    r: BufReader<OwnedReadHalf>,
    noise: Option<(Arc<StatelessTransportState>, u64)>,
}

pub struct FrameWriter {
    w: OwnedWriteHalf,
    noise: Option<(Arc<StatelessTransportState>, u64)>,
}

/// Decodes a base64 API encryption key (the `api: encryption: key:` of the device).
pub fn parse_key(key: &str) -> Result<[u8; 32]> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(key.trim())
        .context("the ESPHome encryption key is not base64")?;
    bytes.try_into().map_err(|_| anyhow!("the ESPHome encryption key must be 32 bytes"))
}

/// Opens the framing on a connected socket. With a key, runs the Noise handshake first; the
/// device's name from its Noise hello is returned when it sends one.
pub async fn open(stream: TcpStream, key: Option<&[u8; 32]>) -> Result<(FrameReader, FrameWriter, Option<String>)> {
    stream.set_nodelay(true)?;
    let (r, w) = stream.into_split();
    let (mut r, mut w) = (BufReader::new(r), w);
    let Some(key) = key else {
        return Ok((FrameReader { r, noise: None }, FrameWriter { w, noise: None }, None));
    };

    let mut hs = snow::Builder::new(NOISE_PATTERN.parse()?).prologue(NOISE_PROLOGUE)?.psk(0, key)?.build_initiator()?;
    let mut buf = vec![0u8; MAX_FRAME];
    let n = hs.write_message(&[], &mut buf)?;
    let mut hello = vec![0x01, 0x00, 0x00];
    let mut handshake = vec![0u8];
    handshake.extend_from_slice(&buf[..n]);
    hello.push(0x01);
    hello.extend(noise_header(handshake.len())?);
    hello.extend(handshake);
    w.write_all(&hello).await?;

    let server_hello = read_noise_frame(&mut r).await.context("waiting for the device's Noise hello")?;
    match server_hello.first() {
        Some(0x01) => {}
        Some(other) => bail!("the device chose an unknown encryption protocol {other}"),
        None => bail!("empty Noise hello from the device"),
    }
    let name = server_hello[1..].split(|&b| b == 0).next().map(|n| String::from_utf8_lossy(n).into_owned());
    let reply = read_noise_frame(&mut r).await.context("waiting for the Noise handshake")?;
    match reply.first() {
        Some(0x00) => {}
        Some(_) => bail!("the device refused the handshake: {}", String::from_utf8_lossy(&reply[1..])),
        None => bail!("empty Noise handshake reply"),
    }
    hs.read_message(&reply[1..], &mut buf).context("Noise handshake failed; is the encryption key right?")?;
    let transport = Arc::new(hs.into_stateless_transport_mode()?);
    Ok((
        FrameReader { r, noise: Some((transport.clone(), 0)) },
        FrameWriter { w, noise: Some((transport, 0)) },
        name.filter(|n| !n.is_empty()),
    ))
}

fn noise_header(len: usize) -> Result<[u8; 2]> {
    if len > MAX_FRAME {
        bail!("frame of {len} bytes is over the Noise limit");
    }
    Ok((len as u16).to_be_bytes())
}

async fn read_noise_frame(r: &mut BufReader<OwnedReadHalf>) -> Result<Vec<u8>> {
    let mut header = [0u8; 3];
    r.read_exact(&mut header).await?;
    if header[0] != 0x01 {
        if header[0] == 0x00 {
            bail!("the device does not use encryption; remove the key");
        }
        bail!("bad frame indicator {:#04x}", header[0]);
    }
    let len = u16::from_be_bytes([header[1], header[2]]) as usize;
    let mut frame = vec![0u8; len];
    r.read_exact(&mut frame).await?;
    Ok(frame)
}

impl FrameReader {
    /// The next message: its type id and payload.
    pub async fn read(&mut self) -> Result<(u16, Vec<u8>)> {
        match &mut self.noise {
            None => {
                let indicator = self.r.read_u8().await?;
                if indicator == 0x01 {
                    bail!("the device requires an encryption key");
                }
                if indicator != 0x00 {
                    bail!("bad frame indicator {indicator:#04x}");
                }
                let len = read_async_varint(&mut self.r).await?;
                let kind = read_async_varint(&mut self.r).await?;
                let mut payload = vec![0u8; len as usize];
                self.r.read_exact(&mut payload).await?;
                Ok((kind as u16, payload))
            }
            Some((transport, nonce)) => {
                let frame = read_noise_frame(&mut self.r).await?;
                let mut plain = vec![0u8; frame.len()];
                let n = transport.read_message(*nonce, &frame, &mut plain).context("decrypting a frame")?;
                *nonce += 1;
                if n < 4 {
                    bail!("encrypted frame too short");
                }
                let kind = u16::from_be_bytes([plain[0], plain[1]]);
                let len = u16::from_be_bytes([plain[2], plain[3]]) as usize;
                let Some(payload) = plain.get(4..4 + len) else { bail!("encrypted frame shorter than its length") };
                Ok((kind, payload.to_vec()))
            }
        }
    }
}

impl FrameWriter {
    pub async fn write(&mut self, kind: u16, payload: &[u8]) -> Result<()> {
        let bytes = match &mut self.noise {
            None => {
                let mut out = vec![0x00];
                out.extend(varint(payload.len() as u64));
                out.extend(varint(kind as u64));
                out.extend_from_slice(payload);
                out
            }
            Some((transport, nonce)) => {
                let mut plain = Vec::with_capacity(payload.len() + 4);
                plain.extend(kind.to_be_bytes());
                plain.extend((payload.len() as u16).to_be_bytes());
                plain.extend_from_slice(payload);
                let mut frame = vec![0u8; plain.len() + TAG];
                let n = transport.write_message(*nonce, &plain, &mut frame)?;
                *nonce += 1;
                let mut out = vec![0x01];
                out.extend(noise_header(n)?);
                out.extend_from_slice(&frame[..n]);
                out
            }
        };
        self.w.write_all(&bytes).await?;
        Ok(())
    }
}

async fn read_async_varint(r: &mut BufReader<OwnedReadHalf>) -> Result<u64> {
    let mut buf = Vec::with_capacity(4);
    loop {
        let b = r.read_u8().await?;
        buf.push(b);
        if b & 0x80 == 0 {
            return Ok(read_varint(&buf, 0)?.0);
        }
        if buf.len() > 10 {
            bail!("varint too long");
        }
    }
}

/// Framing over an already handshaken stream, for the fake device in tests.
#[cfg(test)]
pub(crate) fn from_parts(stream: TcpStream, transport: Arc<StatelessTransportState>) -> (FrameReader, FrameWriter) {
    let (r, w) = stream.into_split();
    (
        FrameReader { r: BufReader::new(r), noise: Some((transport.clone(), 0)) },
        FrameWriter { w, noise: Some((transport, 0)) },
    )
}
