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
//!  <------------------- block frame, length = 20 + stored ------------------->
//! +------+-------+--------+----------+------+--------+--------------+---------+
//! | tag  | codec |  raw   |  stored  | r16  |block_id|   payload    |  xsum   |
//! |  1   |   1   |   4    |    4     |  2   |   4    |    stored    |    4    |
//! +------+-------+--------+----------+------+--------+--------------+---------+
//! +0     +1      +2       +6         +10    +12      +16            +16+stored
//! ```
//! The overhead is amortised across a whole block rather than paid per piece.
//!
//! # Why the frame carries its own id
//!
//! A piece is addressed by BLOCK ID, not by byte offset. Compression is the expensive half of an
//! append, and a physical offset would chain every block to the compressed size of its predecessor —
//! forcing compression onto the serial write path. With logical ids the writer assigns an id the
//! instant a block seals, compression fans out across cores, and blocks land in completion order.
//! Position therefore no longer implies identity, so the id must be in the frame for a tail scan to
//! rebuild the directory. It also means a block can later be moved or recompressed by updating the
//! directory alone, without touching a single part.

use anyhow::{bail, Result};

/// Block anchor. `0b1010_0101`: not `0x00` (a zero-filled torn page tail), not `0xFF` (erased flash),
/// not ASCII, and far in Hamming distance from both.
pub const BLOCK_TAG: u8 = 0xA5;

pub const BLOCK_HDR_LEN: usize = 16;
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
/// Twelve bytes, three u32s. Deliberately absent: the **codec** and the block's **stored length**,
/// because the block header is their only authority — a second copy could disagree.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Loc {
    /// Which block holds this piece. Logical: the fold's block directory maps it to a segment and
    /// offset, so physical placement can change without rewriting any reference.
    pub block_id: u32,
    /// Byte offset of this piece within the block's *decompressed* bytes.
    pub in_off: u32,
    /// This piece's length.
    pub raw: u32,
}

impl Loc {
    pub const WIDTH: usize = 12;

    pub fn encode(&self) -> [u8; Self::WIDTH] {
        let mut b = [0u8; Self::WIDTH];
        b[0..4].copy_from_slice(&self.block_id.to_le_bytes());
        b[4..8].copy_from_slice(&self.in_off.to_le_bytes());
        b[8..12].copy_from_slice(&self.raw.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Loc> {
        if b.len() < Self::WIDTH {
            bail!("Loc needs {} bytes, got {}", Self::WIDTH, b.len());
        }
        Ok(Loc {
            block_id: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            in_off: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            raw: u32::from_le_bytes(b[8..12].try_into().unwrap()),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BlockHdr {
    pub block_id: u32,
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
    let block_id = u32::from_le_bytes(b[12..16].try_into().unwrap());
    Ok(BlockHdr { block_id, codec, raw, stored, r16: [b[10], b[11]] })
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
pub fn encode(out: &mut Vec<u8>, block_id: u32, codec: u8, raw_block: &[u8], payload: &[u8]) -> usize {
    debug_assert!(payload.len() <= raw_block.len(), "encoder must fall back to CODEC_STORED");
    let rh = blake3::hash(raw_block);
    out.clear();
    out.reserve(BLOCK_OVERHEAD + payload.len());
    // `raw` and `stored` are u32 on disk. A silent truncation here writes a frame that decodes to
    // the wrong length, which is corruption a reader cannot distinguish from a bad disk.
    assert!(
        raw_block.len() as u64 <= u32::MAX as u64 && payload.len() as u64 <= u32::MAX as u64,
        "block of {} raw / {} stored bytes exceeds the u32 frame fields; block_target must bound this",
        raw_block.len(),
        payload.len()
    );
    out.push(BLOCK_TAG);
    out.push(codec);
    out.extend_from_slice(&(raw_block.len() as u32).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&rh.as_bytes()[0..2]);
    out.extend_from_slice(&block_id.to_le_bytes());
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
        let l = Loc { block_id: 7, in_off: 1200, raw: 340 };
        assert_eq!(Loc::decode(&l.encode()).unwrap(), l);
        assert_eq!(Loc::WIDTH, 12);
    }

    #[test]
    fn block_roundtrips_and_detects_tears() {
        let raw = "many pieces concatenated into one block. ".repeat(64).into_bytes();
        let mut buf = Vec::new();
        let n = encode(&mut buf, 3, CODEC_STORED, &raw, &raw);
        assert_eq!(n, BLOCK_OVERHEAD + raw.len());

        let hdr = verify_frame_bytes(&buf, false).unwrap();
        assert_eq!(hdr.codec, CODEC_STORED);
        assert_eq!(hdr.raw as usize, raw.len());
        assert_eq!(hdr.r16, blake3::hash(&raw).as_bytes()[0..2]);
        assert_eq!(hdr.block_id, 3);

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
