//! File bytes: packs for small files, content-defined chunks for large ones.
//!
//! Every stored blob is `[codec][data]` where codec 0 = raw, 1 = zstd. Blobs are
//! compressed independently so any range of a pack can be decoded on its own.

use anyhow::{Result, bail};
use bytes::Bytes;

use crate::enc::Writer;
use crate::hash::Hash;

/// Files at or below this size are stored inside the manifest entry.
pub const INLINE_MAX: usize = 64;
/// Files at or above this size are split into chunks instead of packed.
pub const CHUNK_THRESHOLD: usize = 1 << 20;
/// Packs are closed once they reach this size.
pub const PACK_MAX: usize = 64 << 20;
pub const CHUNK_MIN: u32 = 256 << 10;
pub const CHUNK_AVG: u32 = 1 << 20;
pub const CHUNK_MAX: u32 = 4 << 20;

const CODEC_RAW: u8 = 0;
const CODEC_ZSTD: u8 = 1;
const PACK_MAGIC: &[u8; 8] = b"PAILPK1\0";

pub fn encode_blob(raw: &[u8]) -> Vec<u8> {
    if raw.len() >= 128 {
        if let Ok(z) = zstd::bulk::compress(raw, 3) {
            if (z.len() as f64) < raw.len() as f64 * 0.95 {
                let mut out = Vec::with_capacity(z.len() + 1);
                out.push(CODEC_ZSTD);
                out.extend_from_slice(&z);
                return out;
            }
        }
    }
    let mut out = Vec::with_capacity(raw.len() + 1);
    out.push(CODEC_RAW);
    out.extend_from_slice(raw);
    out
}

pub fn decode_blob(stored: &[u8], raw_len: u64) -> Result<Vec<u8>> {
    let Some((&codec, data)) = stored.split_first() else {
        bail!("empty blob")
    };
    match codec {
        CODEC_RAW => Ok(data.to_vec()),
        CODEC_ZSTD => Ok(zstd::bulk::decompress(data, raw_len as usize)?),
        c => bail!("unknown codec {c}"),
    }
}

/// Builds one pack: blobs back to back, then a footer index for GC and fsck.
#[derive(Default)]
pub struct PackBuilder {
    buf: Vec<u8>,
    index: Vec<(Hash, u64, u64, u64)>,
}

impl PackBuilder {
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Append a blob; returns `(offset, stored_len)`.
    pub fn add(&mut self, hash: Hash, raw: &[u8]) -> (u64, u64) {
        let enc = encode_blob(raw);
        let off = self.buf.len() as u64;
        self.buf.extend_from_slice(&enc);
        self.index
            .push((hash, off, enc.len() as u64, raw.len() as u64));
        (off, enc.len() as u64)
    }

    pub fn finish(mut self) -> (Hash, Bytes) {
        let mut w = Writer::default();
        w.varint(self.index.len() as u64);
        for (h, off, len, raw) in &self.index {
            w.hash(h);
            w.varint(*off);
            w.varint(*len);
            w.varint(*raw);
        }
        let footer_len = w.buf.len() as u32;
        self.buf.extend_from_slice(&w.buf);
        self.buf.extend_from_slice(&footer_len.to_le_bytes());
        self.buf.extend_from_slice(PACK_MAGIC);
        let hash = Hash::of(&self.buf);
        (hash, Bytes::from(self.buf))
    }
}

pub fn pack_key(h: &Hash) -> String {
    format!("packs/{}.pack", h.hex())
}

pub fn chunk_key(h: &Hash) -> String {
    format!("chunks/{}", h.hex())
}

/// Split large content into content-defined chunks: `(offset, len)` pairs.
pub fn chunk_spans(data: &[u8]) -> Vec<(usize, usize)> {
    fastcdc::v2020::FastCDC::new(data, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX)
        .map(|c| (c.offset, c.length))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip() {
        for raw in [vec![], b"hello".to_vec(), vec![7u8; 10_000]] {
            let enc = encode_blob(&raw);
            assert_eq!(decode_blob(&enc, raw.len() as u64).unwrap(), raw);
        }
    }

    #[test]
    fn chunks_cover_input() {
        let data: Vec<u8> = (0..5_000_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let spans = chunk_spans(&data);
        assert!(spans.len() > 1);
        let total: usize = spans.iter().map(|s| s.1).sum();
        assert_eq!(total, data.len());
    }
}
