//! Piece compression — the one place zstd is called.
//!
//! The encoder always falls back to [`CODEC_STORED`] when compression does not shrink the input. That
//! single rule makes `stored <= raw` a structural guarantee rather than a usual case, which is what
//! lets `Loc.stored` be a `u32` that can never overflow and makes a fold at most `input + 16 B/piece`.

use super::frame::{CODEC_STORED, CODEC_ZSTD, CODEC_ZSTD_DICT};
use anyhow::{bail, Context, Result};
use std::borrow::Cow;

/// Default compression level. Deliberately a compile-time constant, not configuration: the encoder's
/// output is part of the on-disk bytes, and determinism means no environment may influence it.
pub const LEVEL: i32 = 3;

/// Compress `raw`, choosing the codec. Returns the codec tag and the payload to frame.
///
/// The tie-break is `>=`, not `>`: an equal-size compressed payload is rejected in favour of the
/// stored form, because stored decodes without zstd at all.
pub fn encode<'a>(raw: &'a [u8], dict: Option<&[u8]>) -> Result<(u8, Cow<'a, [u8]>)> {
    let compressed = match dict {
        Some(d) => {
            let mut c = zstd::bulk::Compressor::with_dictionary(LEVEL, d)
                .context("zstd compressor with dictionary")?;
            c.compress(raw).context("zstd dictionary compress")?
        }
        None => zstd::bulk::compress(raw, LEVEL).context("zstd compress")?,
    };
    if compressed.len() >= raw.len() {
        return Ok((CODEC_STORED, Cow::Borrowed(raw)));
    }
    let tag = if dict.is_some() { CODEC_ZSTD_DICT } else { CODEC_ZSTD };
    Ok((tag, Cow::Owned(compressed)))
}

/// Decode one frame's payload back to its exact original bytes.
///
/// `raw_len` comes from the frame header and bounds the output — a payload that decodes to a different
/// length is corruption and fails loud rather than returning short or over-long bytes.
pub fn decode(codec: u8, payload: &[u8], raw_len: u32, dict: Option<&[u8]>) -> Result<Vec<u8>> {
    let n = raw_len as usize;
    let out = match codec {
        CODEC_STORED => {
            if payload.len() != n {
                bail!("stored frame payload {} != raw {}", payload.len(), n);
            }
            payload.to_vec()
        }
        CODEC_ZSTD => zstd::bulk::decompress(payload, n).context("zstd decompress")?,
        CODEC_ZSTD_DICT => {
            let d = dict.ok_or_else(|| anyhow::anyhow!("dictionary frame but no dictionary loaded"))?;
            let mut z = zstd::bulk::Decompressor::with_dictionary(d)
                .context("zstd decompressor with dictionary")?;
            z.decompress(payload, n).context("zstd dictionary decompress")?
        }
        other => bail!("unknown frame codec {other}"),
    };
    if out.len() != n {
        bail!("decoded length {} != declared raw {}", out.len(), n);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressible_input_uses_zstd_and_roundtrips() {
        let raw = "the shared system prompt, repeated. ".repeat(64).into_bytes();
        let (codec, payload) = encode(&raw, None).unwrap();
        assert_eq!(codec, CODEC_ZSTD);
        assert!(payload.len() < raw.len());
        assert_eq!(decode(codec, &payload, raw.len() as u32, None).unwrap(), raw);
    }

    #[test]
    fn incompressible_input_falls_back_to_stored() {
        // Cryptographically random bytes — genuinely incompressible, so the fallback must engage.
        // (A cheap arithmetic sequence is NOT incompressible; zstd finds the pattern.)
        let mut raw: Vec<u8> = Vec::with_capacity(4096);
        let mut seed = [0u8; 32];
        while raw.len() < 4096 {
            seed = blake3::hash(&seed).into();
            raw.extend_from_slice(&seed);
        }
        let (codec, payload) = encode(&raw, None).unwrap();
        assert_eq!(codec, CODEC_STORED, "must fall back rather than store an expanded payload");
        assert_eq!(payload.len(), raw.len(), "stored <= raw is structural");
        assert_eq!(decode(codec, &payload, raw.len() as u32, None).unwrap(), raw);
    }

    #[test]
    fn empty_piece_roundtrips() {
        let raw: Vec<u8> = Vec::new();
        let (codec, payload) = encode(&raw, None).unwrap();
        assert_eq!(codec, CODEC_STORED);
        assert_eq!(decode(codec, &payload, 0, None).unwrap(), raw);
    }

    #[test]
    fn dictionary_roundtrips() {
        let dict = "gen_ai.request.model gen_ai.usage tool_use assistant ".repeat(16).into_bytes();
        let raw = b"{\"gen_ai.request.model\":\"claude\",\"tool_use\":true}".to_vec();
        let (codec, payload) = encode(&raw, Some(&dict)).unwrap();
        // whichever codec won, it must decode byte-exact with the dictionary available
        assert_eq!(decode(codec, &payload, raw.len() as u32, Some(&dict)).unwrap(), raw);
    }

    #[test]
    fn wrong_declared_length_refuses() {
        let raw = b"abcdefghij".to_vec();
        let (codec, payload) = encode(&raw, None).unwrap();
        assert!(decode(codec, &payload, 9, None).is_err(), "length mismatch must fail loud");
    }
}
