//! Wire format for the edge <-> engine audio stream.
//!
//! One UDP datagram carries one 12-byte little-endian header followed by the payload:
//!
//! ```text
//! 0        1        2                 4                                   8                                  12
//! +--------+--------+-----------------+-----------------------------------+-----------------------------------+
//! | version| flags  | session (u16)   | seq (u32)                         | ts_samples (u32)                  |
//! +--------+--------+-----------------+-----------------------------------+-----------------------------------+
//! flags: bits 0-1 = Kind, bits 2-3 = Codec, bits 4-7 reserved (must be zero)
//! ```
//!
//! Audio is 24 kHz mono in 20 ms packets, which divides Mimi's 80 ms frame evenly.
#![cfg_attr(not(test), no_std)]

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 12;

pub const SAMPLE_RATE: u32 = 24_000;
/// Samples in one 20 ms packet.
pub const PACKET_SAMPLES: usize = 480;
/// Samples in one 80 ms Mimi frame.
pub const FRAME_SAMPLES: usize = 1920;
pub const PACKETS_PER_FRAME: usize = FRAME_SAMPLES / PACKET_SAMPLES;

/// Stays under a 1500-byte MTU with room for IP/UDP headers and tunnels.
pub const MAX_PAYLOAD: usize = 1200;
pub const MAX_DATAGRAM: usize = HEADER_LEN + MAX_PAYLOAD;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Opens or re-opens a session. The server answers with a Hello carrying the same session id.
    Hello = 0,
    Audio = 1,
    Bye = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    /// Signed 16-bit little-endian PCM, `PACKET_SAMPLES` samples per packet.
    PcmS16 = 0,
    /// One Opus packet encoding `PACKET_SAMPLES` samples at 24 kHz.
    Opus = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub codec: Codec,
    pub session: u16,
    /// Packet counter. Starts at 0 for the first Audio packet after a Hello, wraps.
    pub seq: u32,
    /// Sample index of the first sample in the payload, wraps.
    pub ts_samples: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Truncated,
    Version(u8),
    Flags(u8),
    PayloadTooLarge(usize),
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "datagram shorter than header"),
            Self::Version(v) => write!(f, "unsupported protocol version {v}"),
            Self::Flags(b) => write!(f, "invalid flags byte {b:#04x}"),
            Self::PayloadTooLarge(n) => write!(f, "payload of {n} bytes exceeds {MAX_PAYLOAD}"),
        }
    }
}

impl Header {
    /// Splits a datagram into header and payload. The payload borrows from `buf`.
    pub fn parse(buf: &[u8]) -> Result<(Header, &[u8]), ParseError> {
        if buf.len() < HEADER_LEN {
            return Err(ParseError::Truncated);
        }
        let (head, payload) = buf.split_at(HEADER_LEN);
        if head[0] != VERSION {
            return Err(ParseError::Version(head[0]));
        }
        let flags = head[1];
        if flags & 0xf0 != 0 {
            return Err(ParseError::Flags(flags));
        }
        let kind = match flags & 0b11 {
            0 => Kind::Hello,
            1 => Kind::Audio,
            2 => Kind::Bye,
            _ => return Err(ParseError::Flags(flags)),
        };
        let codec = match (flags >> 2) & 0b11 {
            0 => Codec::PcmS16,
            1 => Codec::Opus,
            _ => return Err(ParseError::Flags(flags)),
        };
        if payload.len() > MAX_PAYLOAD {
            return Err(ParseError::PayloadTooLarge(payload.len()));
        }
        let header = Header {
            kind,
            codec,
            session: u16::from_le_bytes([head[2], head[3]]),
            seq: u32::from_le_bytes([head[4], head[5], head[6], head[7]]),
            ts_samples: u32::from_le_bytes([head[8], head[9], head[10], head[11]]),
        };
        Ok((header, payload))
    }

    /// Writes the header into the first `HEADER_LEN` bytes of `buf`.
    ///
    /// Panics if `buf` is shorter than `HEADER_LEN`.
    pub fn write(&self, buf: &mut [u8]) {
        buf[0] = VERSION;
        buf[1] = (self.kind as u8) | ((self.codec as u8) << 2);
        buf[2..4].copy_from_slice(&self.session.to_le_bytes());
        buf[4..8].copy_from_slice(&self.seq.to_le_bytes());
        buf[8..12].copy_from_slice(&self.ts_samples.to_le_bytes());
    }
}

/// Copies little-endian s16 bytes into samples. Returns the number of samples written.
pub fn pcm_from_bytes(bytes: &[u8], out: &mut [i16]) -> usize {
    let n = (bytes.len() / 2).min(out.len());
    for (sample, pair) in out[..n].iter_mut().zip(bytes.as_chunks::<2>().0) {
        *sample = i16::from_le_bytes(*pair);
    }
    n
}

/// Copies samples into little-endian s16 bytes. Returns the number of bytes written.
pub fn pcm_to_bytes(samples: &[i16], out: &mut [u8]) -> usize {
    let n = samples.len().min(out.len() / 2);
    for (pair, sample) in out.as_chunks_mut::<2>().0.iter_mut().zip(&samples[..n]) {
        *pair = sample.to_le_bytes();
    }
    n * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let header = Header {
            kind: Kind::Audio,
            codec: Codec::Opus,
            session: 0xbeef,
            seq: 0xdead_0001,
            ts_samples: 480 * 7,
        };
        let mut buf = [0u8; HEADER_LEN + 3];
        header.write(&mut buf);
        buf[HEADER_LEN..].copy_from_slice(&[1, 2, 3]);
        let (parsed, payload) = Header::parse(&buf).unwrap();
        assert_eq!(parsed, header);
        assert_eq!(payload, &[1, 2, 3]);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Header::parse(&[0u8; 5]), Err(ParseError::Truncated));
        let mut buf = [0u8; HEADER_LEN];
        buf[0] = 9;
        assert_eq!(Header::parse(&buf), Err(ParseError::Version(9)));
        buf[0] = VERSION;
        buf[1] = 0b11;
        assert_eq!(Header::parse(&buf), Err(ParseError::Flags(0b11)));
        buf[1] = 0x10;
        assert_eq!(Header::parse(&buf), Err(ParseError::Flags(0x10)));
        let big = [0u8; MAX_DATAGRAM + 1];
        let mut big = big;
        big[0] = VERSION;
        assert_eq!(Header::parse(&big), Err(ParseError::PayloadTooLarge(MAX_PAYLOAD + 1)));
    }

    #[test]
    fn pcm_round_trip() {
        let samples = [0i16, 1, -1, i16::MAX, i16::MIN];
        let mut bytes = [0u8; 10];
        assert_eq!(pcm_to_bytes(&samples, &mut bytes), 10);
        let mut back = [0i16; 5];
        assert_eq!(pcm_from_bytes(&bytes, &mut back), 5);
        assert_eq!(back, samples);
    }
}
