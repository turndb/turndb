//! The piece frame and `Loc` — the fold's addressable unit.
//!
//! A frame is **individually decodable**: nothing outside it is needed except the segment's trained
//! dictionary (when `codec == ZSTD_DICT`), which the segment header names. That is what buys the
//! read-at-ratio property — a point read decompresses one piece, not a block.
//!
//! ```text
//!  <---------------- frame, length = 16 + stored ----------------->
//! +------+-------+--------+----------+------+------------+---------+
//! | tag  | codec |  raw   |  stored  | h16  |  payload   |  xsum   |
//! |  1   |   1   |   4    |    4     |  2   |   stored   |    4    |
//! +------+-------+--------+----------+------+------------+---------+
//! +0     +1      +2       +6         +10    +12          +12+stored
//! ```

use crate::types::PieceHash;
use anyhow::{bail, Result};

/// Frame anchor. `0b1010_0101`: not `0x00` (a zero-filled torn page tail), not `0xFF` (erased flash),
/// not ASCII, and far in Hamming distance from both — so a torn tail is unlikely to look like a frame.
pub const FRAME_TAG: u8 = 0xA5;

pub const FRAME_HDR_LEN: usize = 12;
pub const FRAME_XSUM_LEN: usize = 4;
/// Bytes a frame costs beyond its payload.
pub const FRAME_OVERHEAD: usize = FRAME_HDR_LEN + FRAME_XSUM_LEN;

/// Payload is the raw bytes verbatim.
pub const CODEC_STORED: u8 = 0;
/// Payload is one complete standard zstd frame.
pub const CODEC_ZSTD: u8 = 1;
/// Payload is one zstd frame encoded against this segment's trained dictionary.
pub const CODEC_ZSTD_DICT: u8 = 2;

/// Where a piece lives and what it costs to read — the value a part's hot dictionary column stores,
/// one per distinct piece. Sixteen bytes, four u32s, naturally aligned.
///
/// Deliberately absent: the **codec**, because the frame header is its only authority (a second copy
/// could disagree); and the **dict id**, because the segment header names it. `off` is `u32` because
/// `seg_max <= 4 GiB` bounds every frame start — see [`super::segment::SEG_MAX_LIMIT`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Loc {
    pub seg: u32,
    /// Offset of the frame's `tag` byte within the segment. Always `>= SEG_HDR_LEN`.
    pub off: u32,
    /// Payload length — makes a read exactly one positioned read of `16 + stored` bytes.
    pub stored: u32,
    /// Uncompressed length — lets a reader size its output buffer before touching disk.
    pub raw: u32,
}

impl Loc {
    pub const WIDTH: usize = 16;

    /// Total on-disk bytes of this piece's frame, computable with no disk access.
    pub fn frame_len(&self) -> u64 {
        FRAME_OVERHEAD as u64 + self.stored as u64
    }

    /// Byte offset just past this frame.
    pub fn end(&self) -> u64 {
        self.off as u64 + self.frame_len()
    }

    pub fn encode(&self) -> [u8; Self::WIDTH] {
        let mut b = [0u8; Self::WIDTH];
        b[0..4].copy_from_slice(&self.seg.to_le_bytes());
        b[4..8].copy_from_slice(&self.off.to_le_bytes());
        b[8..12].copy_from_slice(&self.stored.to_le_bytes());
        b[12..16].copy_from_slice(&self.raw.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Loc> {
        if b.len() < Self::WIDTH {
            bail!("Loc needs {} bytes, got {}", Self::WIDTH, b.len());
        }
        Ok(Loc {
            seg: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            off: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            stored: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            raw: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        })
    }
}

/// A parsed frame header. Field-validated on construction — an invalid header never becomes one.
#[derive(Clone, Copy, Debug)]
pub struct FrameHdr {
    pub codec: u8,
    pub raw: u32,
    pub stored: u32,
    /// First two bytes of the content's BLAKE3 — a free self-check and a cheap prefilter. It is
    /// **never** used to conclude equality; identity is the full 32-byte hash.
    pub h16: [u8; 2],
}

/// Parse + validate the fixed 12-byte header. `seg_has_dict` lets the parser reject a dictionary-coded
/// frame in a segment that names no dictionary — unreadable by construction, so it must not be accepted.
pub fn parse_hdr(b: &[u8], seg_has_dict: bool) -> Result<FrameHdr> {
    if b.len() < FRAME_HDR_LEN {
        bail!("frame header truncated: {} bytes", b.len());
    }
    if b[0] != FRAME_TAG {
        bail!("frame tag {:#04x} != {:#04x}", b[0], FRAME_TAG);
    }
    let codec = b[1];
    if codec > CODEC_ZSTD_DICT {
        bail!("unknown frame codec {codec}");
    }
    let raw = u32::from_le_bytes(b[2..6].try_into().unwrap());
    let stored = u32::from_le_bytes(b[6..10].try_into().unwrap());
    // Guaranteed by the encoder's codec-0 fallback, so a violation means corruption.
    if stored > raw {
        bail!("frame stored {stored} > raw {raw}");
    }
    if codec == CODEC_STORED && raw != stored {
        bail!("stored-codec frame with raw {raw} != stored {stored}");
    }
    if codec == CODEC_ZSTD_DICT && !seg_has_dict {
        bail!("dictionary-coded frame in a segment that names no dictionary");
    }
    Ok(FrameHdr { codec, raw, stored, h16: [b[10], b[11]] })
}

/// The 4-byte tail checksum over `frame[0 .. FRAME_HDR_LEN + stored]`.
///
/// BLAKE3 already identifies *content*, so this is not about content integrity — it is the only thing
/// that can tell a **torn write** (a frame whose promised payload never fully reached disk) from a good
/// one during tail recovery, before any decode is attempted.
pub fn xsum(frame_prefix: &[u8]) -> [u8; FRAME_XSUM_LEN] {
    let h = blake3::hash(frame_prefix);
    let b = h.as_bytes();
    [b[0], b[1], b[2], b[3]]
}

/// Build a complete frame into `out` (cleared first). Returns the frame length.
pub fn encode(out: &mut Vec<u8>, codec: u8, raw_len: u32, payload: &[u8], hash: &PieceHash) -> usize {
    debug_assert!(payload.len() as u64 <= raw_len as u64, "encoder must fall back to CODEC_STORED");
    out.clear();
    out.reserve(FRAME_OVERHEAD + payload.len());
    out.push(FRAME_TAG);
    out.push(codec);
    out.extend_from_slice(&raw_len.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&hash.0[0..2]);
    out.extend_from_slice(payload);
    let sum = xsum(&out[..]);
    out.extend_from_slice(&sum);
    out.len()
}

/// Verify a whole frame's bytes (header + payload + xsum) without decoding the payload.
pub fn verify_frame_bytes(frame: &[u8], seg_has_dict: bool) -> Result<FrameHdr> {
    let hdr = parse_hdr(frame, seg_has_dict)?;
    let end = FRAME_HDR_LEN + hdr.stored as usize;
    if frame.len() < end + FRAME_XSUM_LEN {
        bail!("frame truncated: have {} bytes, need {}", frame.len(), end + FRAME_XSUM_LEN);
    }
    if xsum(&frame[..end]) != frame[end..end + FRAME_XSUM_LEN] {
        bail!("frame checksum mismatch (torn write or corruption)");
    }
    Ok(hdr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loc_roundtrips() {
        let l = Loc { seg: 7, off: 48, stored: 1234, raw: 5678 };
        assert_eq!(Loc::decode(&l.encode()).unwrap(), l);
        assert_eq!(l.frame_len(), 16 + 1234);
        assert_eq!(l.end(), 48 + 16 + 1234);
    }

    #[test]
    fn frame_roundtrips_and_detects_tears() {
        let raw = b"the quick brown fox jumps over the lazy dog".to_vec();
        let h = PieceHash::of(&raw);
        let mut buf = Vec::new();
        let n = encode(&mut buf, CODEC_STORED, raw.len() as u32, &raw, &h);
        assert_eq!(n, FRAME_OVERHEAD + raw.len());

        let hdr = verify_frame_bytes(&buf, false).unwrap();
        assert_eq!(hdr.codec, CODEC_STORED);
        assert_eq!(hdr.raw as usize, raw.len());
        assert_eq!(hdr.h16, [h.0[0], h.0[1]]);

        // a flipped payload byte is caught by the xsum
        let mut torn = buf.clone();
        let mid = FRAME_HDR_LEN + 3;
        torn[mid] ^= 0x01;
        assert!(verify_frame_bytes(&torn, false).is_err());

        // a truncated tail is caught before any decode
        assert!(verify_frame_bytes(&buf[..buf.len() - 1], false).is_err());
    }

    #[test]
    fn invalid_headers_refuse() {
        let mut b = vec![0u8; FRAME_HDR_LEN];
        b[0] = 0x00;
        assert!(parse_hdr(&b, false).is_err(), "zero tag must refuse");

        b[0] = FRAME_TAG;
        b[1] = 9;
        assert!(parse_hdr(&b, false).is_err(), "unknown codec must refuse");

        // dictionary codec in a dictionary-less segment
        b[1] = CODEC_ZSTD_DICT;
        b[2..6].copy_from_slice(&10u32.to_le_bytes());
        b[6..10].copy_from_slice(&5u32.to_le_bytes());
        assert!(parse_hdr(&b, false).is_err());
        assert!(parse_hdr(&b, true).is_ok());

        // stored > raw is corruption
        b[1] = CODEC_ZSTD;
        b[2..6].copy_from_slice(&5u32.to_le_bytes());
        b[6..10].copy_from_slice(&10u32.to_le_bytes());
        assert!(parse_hdr(&b, false).is_err());
    }
}
