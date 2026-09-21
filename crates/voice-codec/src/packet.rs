use anyhow::{bail, Result};
use voice_proto::{pcm_from_bytes, pcm_to_bytes, Codec, PACKET_SAMPLES};

pub type PacketPcm = [i16; PACKET_SAMPLES];

/// Decodes one 20 ms wire packet into PCM without allocating.
///
/// Only PCM is implemented. The pure-Rust `opus-rs` 0.1.33 decodes 24 kHz streams 1.8-2.6x too
/// loud (16 and 48 kHz are correct), and libopus needs cmake, so `Codec::Opus` stays a reserved
/// wire value until one of those changes. PCM costs 384 kbit/s and has no algorithmic delay.
pub enum PacketDecoder {
    Pcm,
}

impl PacketDecoder {
    pub fn new(codec: Codec) -> Result<Self> {
        match codec {
            Codec::PcmS16 => Ok(Self::Pcm),
            Codec::Opus => bail!("opus is not implemented"),
        }
    }

    pub fn codec(&self) -> Codec {
        match self {
            Self::Pcm => Codec::PcmS16,
        }
    }

    pub fn decode(&mut self, payload: &[u8], out: &mut PacketPcm) -> Result<()> {
        match self {
            Self::Pcm => {
                if payload.len() != PACKET_SAMPLES * 2 {
                    bail!("pcm payload is {} bytes, expected {}", payload.len(), PACKET_SAMPLES * 2);
                }
                pcm_from_bytes(payload, out);
            }
        }
        Ok(())
    }

    /// Fills `out` for a packet that never arrived. PCM has no predictor state, so this is silence.
    pub fn conceal(&mut self, out: &mut PacketPcm) {
        out.fill(0);
    }
}

/// Encodes one 20 ms PCM packet for the wire without allocating.
pub enum PacketEncoder {
    Pcm,
}

impl PacketEncoder {
    pub fn new(codec: Codec) -> Result<Self> {
        match codec {
            Codec::PcmS16 => Ok(Self::Pcm),
            Codec::Opus => bail!("opus is not implemented"),
        }
    }

    /// Returns the payload length written to `out`.
    pub fn encode(&mut self, pcm: &PacketPcm, out: &mut [u8]) -> Result<usize> {
        match self {
            Self::Pcm => {
                if out.len() < PACKET_SAMPLES * 2 {
                    bail!("output buffer too small for a pcm packet");
                }
                Ok(pcm_to_bytes(pcm, out))
            }
        }
    }
}

pub fn f32_to_i16(src: &[f32], dst: &mut [i16]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_round_trip() {
        let mut enc = PacketEncoder::new(Codec::PcmS16).unwrap();
        let mut dec = PacketDecoder::new(Codec::PcmS16).unwrap();
        let mut wire = [0u8; 1200];
        let mut pcm = [0i16; PACKET_SAMPLES];
        pcm.iter_mut().enumerate().for_each(|(i, s)| *s = (i as i16 - 240) * 100);
        let n = enc.encode(&pcm, &mut wire).unwrap();
        let mut back = [0i16; PACKET_SAMPLES];
        dec.decode(&wire[..n], &mut back).unwrap();
        assert_eq!(pcm, back);
        assert!(dec.decode(&wire[..10], &mut back).is_err());
        assert!(PacketDecoder::new(Codec::Opus).is_err());
    }
}
