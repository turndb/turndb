//! The block frame and `Loc` — the fold's storage and addressing units.
//!
//! A **block** holds many pieces and is compressed as a unit. A **piece** is addressed inside it.
//! Separating the two is the whole point: compressing per piece throws away the cross-piece redundancy
//! that dominates trace data (near-identical messages, shared JSON scaffolding), and it makes reading a
//! record pay one decompression setup per piece when a record's pieces were captured together and land
//! in a handful of blocks. Measured on two real corpora, blocking at this size is both smaller *and*
//! faster to read than per-piece framing.
//!
//! A block containing exactly one piece IS a per-piece frame, so this format subsumes that one rather
//! than replacing it — nothing is foreclosed.
//!
//! ```text
//!  <---------------- block frame, length = 16 + stored ---------------->
//! +------+-------+--------+----------+------+--------------+---------+
//! | tag  | codec |  raw   |  stored  | r16  |   payload    |  xsum   |
//! |  1   |   1   |   4    |    4     |  2   |    stored    |    4    |
//! +------+-------+--------+----------+------+--------------+---------+
//! +0     +1      +2       +6         +10    +12            +12+stored
//! ```
//! The 16 bytes of overhead are now amortised across a whole block rather than paid per piece.

use anyhow::{bail, Result};

/// Block anchor. `0b1010_0101`: not `0x00` (a zero-filled torn page tail), not `0xFF` (erased flash),
/// not ASCII, and far in Hamming distance from both.
pub const BLOCK_TAG: u8 = 0xA5;

pub const BLOCK_HDR_LEN: usize = 12;
pub const BLOCK_XSUM_LEN: usize = 4;
pub const BLOCK_OVERHEAD: usize = BLOCK_HDR_LEN + BLOCK_XSUM_LEN;

/// Payload is the raw block bytes verbatim.
pub const CODEC_STORED: u8 = 0;
/// Payload is one complete standard zstd frame.
pub const CODEC_ZSTD: u8 = 1;
/// Payload is one zstd frame encoded against this segment's trained dictionary.
pub const CODEC_ZSTD_DICT: u8 = 2;

/// Default raw content gathered before a block seals.
///
/// A WRITE-SIDE knob only: the block header carries `raw`, `stored` and `codec`, so a reader never
/// needs to know what the writer chose, and changing it is neither a format change nor a
/// compatibility question. Byte-identity holds for a given setting. Larger blocks compress harder and
/// cost more per read; see `FoldCfg`. Default is compression-first (owner preference): 4 MiB.
pub const BLOCK_TARGET_DEFAULT: usize = 4 * 1024 * 1024;

/// Where a piece lives: which block, and where inside it.
///
/// Sixteen bytes, four u32s. Deliberately absent: the **codec** and the block's **stored length**,
/// because the block header is their only authority — a second copy could disagree.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Loc {
    pub seg: u32,
    /// Byte offset of the block frame's `tag` within the segment.
    pub block_off: u32,
    /// Byte offset of this piece within the block's *decompressed* bytes.
    pub in_off: u32,
    /// This piece's length.
    pub raw: u32,
}

impl Loc {
    pub const WIDTH: usize = 16;

    pub fn encode(&self) -> [u8; Self::WIDTH] {
        let mut b = [0u8; Self::WIDTH];
        b[0..4].copy_from_slice(&self.seg.to_le_bytes());
        b[4..8].copy_from_slice(&self.block_off.to_le_bytes());
        b[8..12].copy_from_slice(&self.in_off.to_le_bytes());
        b[12..16].copy_from_slice(&self.raw.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Loc> {
        if b.len() < Self::WIDTH {
            bail!("Loc needs {} bytes, got {}", Self::WIDTH, b.len());
        }
        Ok(Loc {
            seg: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            block_off: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            in_off: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            raw: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        })
    }

    /// The key a block cache is indexed by — every piece in one block shares it.
    pub fn block_key(&self) -> (u32, u32) {
        (self.seg, self.block_off)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BlockHdr {
    pub codec: u8,
    /// Decompressed size of the whole block.
    pub raw: u32,
    /// On-disk payload size.
    pub stored: u32,
    /// First two bytes of BLAKE3 over the block's raw bytes — a free check that a decode produced the
    /// bytes this block was written for. It filters; it never concludes identity.
    pub r16: [u8; 2],
}

impl BlockHdr {
    pub fn frame_len(&self) -> u64 {
        BLOCK_OVERHEAD as u64 + self.stored as u64
    }
}

pub fn parse_hdr(b: &[u8], seg_has_dict: bool) -> Result<BlockHdr> {
    if b.len() < BLOCK_HDR_LEN {
        bail!("block header truncated: {} bytes", b.len());
    }
    if b[0] != BLOCK_TAG {
        bail!("block tag {:#04x} != {:#04x}", b[0], BLOCK_TAG);
    }
    let codec = b[1];
    if codec > CODEC_ZSTD_DICT {
        bail!("unknown block codec {codec}");
    }
    let raw = u32::from_le_bytes(b[2..6].try_into().unwrap());
    let stored = u32::from_le_bytes(b[6..10].try_into().unwrap());
    // Guaranteed by the encoder's codec-0 fallback, so a violation means corruption.
    if stored > raw {
        bail!("block stored {stored} > raw {raw}");
    }
    if codec == CODEC_STORED && raw != stored {
        bail!("stored-codec block with raw {raw} != stored {stored}");
    }
    if codec == CODEC_ZSTD_DICT && !seg_has_dict {
        bail!("dictionary-coded block in a segment that names no dictionary");
    }
    Ok(BlockHdr { codec, raw, stored, r16: [b[10], b[11]] })
}

/// The 4-byte tail checksum over `frame[0 .. BLOCK_HDR_LEN + stored]`.
///
/// Not about content integrity — BLAKE3 identifies content. This is the only thing that can tell a
/// **torn write** from a good block during tail recovery, before any decode is attempted.
pub fn xsum(frame_prefix: &[u8]) -> [u8; BLOCK_XSUM_LEN] {
    let h = blake3::hash(frame_prefix);
    let b = h.as_bytes();
    [b[0], b[1], b[2], b[3]]
}

/// Build a complete block frame into `out` (cleared first). `raw_block` is the uncompressed block.
pub fn encode(out: &mut Vec<u8>, codec: u8, raw_block: &[u8], payload: &[u8]) -> usize {
    debug_assert!(payload.len() <= raw_block.len(), "encoder must fall back to CODEC_STORED");
    let rh = blake3::hash(raw_block);
    out.clear();
    out.reserve(BLOCK_OVERHEAD + payload.len());
    out.push(BLOCK_TAG);
    out.push(codec);
    out.extend_from_slice(&(raw_block.len() as u32).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&rh.as_bytes()[0..2]);
    out.extend_from_slice(payload);
    let sum = xsum(&out[..]);
    out.extend_from_slice(&sum);
    out.len()
}

/// Verify a whole block frame's bytes without decoding the payload.
pub fn verify_frame_bytes(frame: &[u8], seg_has_dict: bool) -> Result<BlockHdr> {
    let hdr = parse_hdr(frame, seg_has_dict)?;
    let end = BLOCK_HDR_LEN + hdr.stored as usize;
    if frame.len() < end + BLOCK_XSUM_LEN {
        bail!("block truncated: have {} bytes, need {}", frame.len(), end + BLOCK_XSUM_LEN);
    }
    if xsum(&frame[..end]) != frame[end..end + BLOCK_XSUM_LEN] {
        bail!("block checksum mismatch (torn write or corruption)");
    }
    Ok(hdr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loc_roundtrips() {
        let l = Loc { seg: 7, block_off: 48, in_off: 1200, raw: 340 };
        assert_eq!(Loc::decode(&l.encode()).unwrap(), l);
        assert_eq!(l.block_key(), (7, 48));
    }

    #[test]
    fn block_roundtrips_and_detects_tears() {
        let raw = "many pieces concatenated into one block. ".repeat(64).into_bytes();
        let mut buf = Vec::new();
        let n = encode(&mut buf, CODEC_STORED, &raw, &raw);
        assert_eq!(n, BLOCK_OVERHEAD + raw.len());

        let hdr = verify_frame_bytes(&buf, false).unwrap();
        assert_eq!(hdr.codec, CODEC_STORED);
        assert_eq!(hdr.raw as usize, raw.len());
        assert_eq!(hdr.r16, blake3::hash(&raw).as_bytes()[0..2]);

        let mut torn = buf.clone();
        torn[BLOCK_HDR_LEN + 3] ^= 0x01;
        assert!(verify_frame_bytes(&torn, false).is_err(), "a flipped payload byte must be caught");
        assert!(verify_frame_bytes(&buf[..buf.len() - 1], false).is_err(), "a truncated tail must be caught");
    }

    #[test]
    fn invalid_headers_refuse() {
        let mut b = vec![0u8; BLOCK_HDR_LEN];
        assert!(parse_hdr(&b, false).is_err(), "zero tag must refuse");
        b[0] = BLOCK_TAG;
        b[1] = 9;
        assert!(parse_hdr(&b, false).is_err(), "unknown codec must refuse");

        b[1] = CODEC_ZSTD_DICT;
        b[2..6].copy_from_slice(&10u32.to_le_bytes());
        b[6..10].copy_from_slice(&5u32.to_le_bytes());
        assert!(parse_hdr(&b, false).is_err(), "dict codec needs a segment dictionary");
        assert!(parse_hdr(&b, true).is_ok());

        b[1] = CODEC_ZSTD;
        b[2..6].copy_from_slice(&5u32.to_le_bytes());
        b[6..10].copy_from_slice(&10u32.to_le_bytes());
        assert!(parse_hdr(&b, false).is_err(), "stored > raw is corruption");
    }
}
