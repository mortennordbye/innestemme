//! Decoding side of a SentencePiece model: the id -> piece table read straight from the protobuf.
//! Avoids the `sentencepiece` crate, which builds the C++ library with cmake.

use std::path::Path;

use anyhow::{bail, Context, Result};

pub struct Pieces(Vec<String>);

impl Pieces {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&bytes)
    }

    /// `ModelProto.pieces` is field 1; inside each `SentencePiece`, the piece text is field 1.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut pieces = Vec::new();
        let mut reader = Reader(bytes);
        while let Some((field, value)) = reader.next()? {
            if let (1, Value::Bytes(message)) = (field, value) {
                let mut inner = Reader(message);
                let mut piece = String::new();
                while let Some((field, value)) = inner.next()? {
                    if let (1, Value::Bytes(text)) = (field, value) {
                        piece = String::from_utf8_lossy(text).into_owned();
                    }
                }
                pieces.push(piece);
            }
        }
        if pieces.is_empty() {
            bail!("no pieces in sentencepiece model");
        }
        Ok(Self(pieces))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Joins the pieces; `▁` marks a word boundary and `<0xNN>` pieces are raw bytes.
    pub fn decode(&self, ids: &[u32]) -> String {
        let mut bytes = Vec::new();
        for &id in ids {
            let Some(piece) = self.0.get(id as usize) else {
                continue;
            };
            match piece.strip_prefix("<0x").and_then(|p| p.strip_suffix('>')) {
                Some(hex) if hex.len() == 2 => bytes.extend(u8::from_str_radix(hex, 16).ok()),
                _ if piece.starts_with('<') && piece.ends_with('>') => {}
                _ => bytes.extend_from_slice(piece.replace('\u{2581}', " ").as_bytes()),
            }
        }
        String::from_utf8_lossy(&bytes).trim().to_owned()
    }
}

enum Value<'a> {
    Int,
    Bytes(&'a [u8]),
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.0.split_first().context("truncated varint")?;
            self.0 = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("varint too long")
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            bail!("truncated field");
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn next(&mut self) -> Result<Option<(u64, Value<'a>)>> {
        if self.0.is_empty() {
            return Ok(None);
        }
        let tag = self.varint()?;
        let value = match tag & 7 {
            0 => {
                self.varint()?;
                Value::Int
            }
            1 => {
                self.take(8)?;
                Value::Int
            }
            2 => {
                let len = self.varint()? as usize;
                Value::Bytes(self.take(len)?)
            }
            5 => {
                self.take(4)?;
                Value::Int
            }
            wire => bail!("unsupported protobuf wire type {wire}"),
        };
        Ok(Some((tag >> 3, value)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(text: &str) -> Vec<u8> {
        let mut inner = vec![0x0a, text.len() as u8];
        inner.extend_from_slice(text.as_bytes());
        inner.extend_from_slice(&[0x15, 0, 0, 0x80, 0x3f, 0x18, 0x01]);
        let mut outer = vec![0x0a, inner.len() as u8];
        outer.extend(inner);
        outer
    }

    #[test]
    fn decodes_word_pieces_and_bytes() {
        let mut model = Vec::new();
        for text in ["<unk>", "\u{2581}hey", "\u{2581}ed", "en", "<0x21>"] {
            model.extend(piece(text));
        }
        // An unrelated trailing field, like TrainerSpec.
        model.extend_from_slice(&[0x12, 0x02, 0x08, 0x01]);
        let pieces = Pieces::parse(&model).unwrap();
        assert_eq!(pieces.len(), 5);
        assert_eq!(pieces.decode(&[1, 2, 3, 4]), "hey eden!");
        assert_eq!(pieces.decode(&[0, 2, 3]), "eden");
    }
}
