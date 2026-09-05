//! Block compression — the one place zstd is called.
//!
//! The encoder always falls back to [`CODEC_STORED`] when compression does not shrink the input. That
//! single rule makes `stored <= raw` a structural guarantee rather than a usual case, which is what
//! lets `Loc.stored` be a `u32` that can never overflow and makes a fold at most `input + 16 B/piece`.
//!
//! # Two backends, one format
//!
//! Native builds call the C zstd library. `wasm32` builds call a pure-Rust encoder, because
//! requiring a C toolchain to build for WASM would defeat the point of shipping a single portable
//! artifact. **This is a build-time choice with no format consequence**: both emit ordinary zstd
//! frames, each reads the other's byte-exact (measured, including dictionary frames), and the codec
//! tag in the block header says nothing about which produced it. A store written by a WASM build is
//! an ordinary store.
//!
//! The split lives in the private `z` module and nowhere else, so the fallback policy above is
//! written once. (Not a doc link: `z` is private, and a public page must not link into it.)

use super::block::{CODEC_STORED, CODEC_ZSTD, CODEC_ZSTD_DICT};
use anyhow::{bail, Result};
use std::borrow::Cow;

/// The zstd backend: C on native, pure Rust on wasm32. Same frames either way.
mod z {
    #[cfg(not(target_arch = "wasm32"))]
    use anyhow::Context;
    use anyhow::Result;

    #[cfg(not(target_arch = "wasm32"))]
    pub fn compress(raw: &[u8], dict: Option<&[u8]>, level: i32) -> Result<Vec<u8>> {
        match dict {
            Some(d) => zstd::bulk::Compressor::with_dictionary(level, d)
                .context("zstd compressor with dictionary")?
                .compress(raw)
                .context("zstd dictionary compress"),
            None => zstd::bulk::compress(raw, level).context("zstd compress"),
        }
    }

    /// Decode into a buffer sized by the block header. The caller-supplied length is the bound: a
    /// frame claiming more than `out` holds fails here rather than allocating on a corrupt header.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn decompress_into(payload: &[u8], dict: Option<&[u8]>, out: &mut [u8]) -> Result<usize> {
        let v = match dict {
            Some(d) => zstd::bulk::Decompressor::with_dictionary(d)
                .context("zstd decompressor with dictionary")?
                .decompress(payload, out.len())
                .context("zstd dictionary decompress")?,
            None => zstd::bulk::decompress(payload, out.len()).context("zstd decompress")?,
        };
        if v.len() > out.len() {
            anyhow::bail!("decoded {} exceeds declared {}", v.len(), out.len());
        }
        out[..v.len()].copy_from_slice(&v);
        Ok(v.len())
    }

    #[cfg(target_arch = "wasm32")]
    pub fn compress(raw: &[u8], dict: Option<&[u8]>, level: i32) -> Result<Vec<u8>> {
        use structured_zstd::encoding::{CompressionLevel, FrameCompressor};
        match dict {
            Some(d) => {
                let mut c: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
                c.set_dictionary_from_bytes(d)
                    .map_err(|e| anyhow::anyhow!("zstd compressor with dictionary: {e:?}"))?;
                Ok(c.compress_independent_frame(raw))
            }
            None => Ok(structured_zstd::encoding::compress_slice_to_vec(
                raw,
                CompressionLevel::Level(level),
            )),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn decompress_into(payload: &[u8], dict: Option<&[u8]>, out: &mut [u8]) -> Result<usize> {
        let mut d = structured_zstd::decoding::FrameDecoder::new();
        if let Some(dict) = dict {
            d.add_dict_from_bytes(dict)
                .map_err(|e| anyhow::anyhow!("zstd decompressor with dictionary: {e:?}"))?;
        }
        // `decode_all` writes into the caller's slice and refuses to overrun it, so the block
        // header's `raw_len` bounds the output directly — no growable buffer to size from a
        // possibly-corrupt frame.
        d.decode_all(payload, out).map_err(|e| anyhow::anyhow!("zstd decompress: {e:?}"))
    }
}

/// Default compression level. A WRITE-SIDE knob: the codec tag rides in the block header, so the
/// reader is indifferent to it. Higher levels cost write throughput and buy size; decompression speed
/// is barely affected. Default is compression-first (owner preference): 19.
pub const LEVEL_DEFAULT: i32 = 19;

/// Compress `raw`, choosing the codec. Returns the codec tag and the payload to frame.
///
/// The tie-break is `>=`, not `>`: an equal-size compressed payload is rejected in favour of the
/// stored form, because stored decodes without zstd at all.
pub fn encode<'a>(raw: &'a [u8], dict: Option<&[u8]>, level: i32) -> Result<(u8, Cow<'a, [u8]>)> {
    let compressed = z::compress(raw, dict, level)?;
    if compressed.len() >= raw.len() {
        return Ok((CODEC_STORED, Cow::Borrowed(raw)));
    }
    let tag = if dict.is_some() { CODEC_ZSTD_DICT } else { CODEC_ZSTD };
    Ok((tag, Cow::Owned(compressed)))
}

/// Decode one block's payload back to its exact original bytes.
///
/// `raw_len` comes from the block header and bounds the output — a payload that decodes to a different
/// length is corruption and fails loud rather than returning short or over-long bytes.
pub fn decode(codec: u8, payload: &[u8], raw_len: u32, dict: Option<&[u8]>) -> Result<Vec<u8>> {
    let n = usize::try_from(raw_len).map_err(|_| {
        anyhow::anyhow!("declared decoded length {raw_len} exceeds this process's address space")
    })?;
    match codec {
        CODEC_STORED => {
            if payload.len() != n {
                bail!("stored block payload {} != raw {}", payload.len(), n);
            }
            let mut out = Vec::new();
            out.try_reserve_exact(n)
                .map_err(|_| anyhow::anyhow!("cannot allocate {n} decoded fold bytes"))?;
            out.extend_from_slice(payload);
            Ok(out)
        }
        CODEC_ZSTD | CODEC_ZSTD_DICT => {
            let d =
                if codec == CODEC_ZSTD_DICT {
                    Some(dict.ok_or_else(|| {
                        anyhow::anyhow!("dictionary block but no dictionary loaded")
                    })?)
                } else {
                    None
                };
            let mut out = Vec::new();
            out.try_reserve_exact(n)
                .map_err(|_| anyhow::anyhow!("cannot allocate {n} decoded fold bytes"))?;
            out.resize(n, 0);
            let got = z::decompress_into(payload, d, &mut out)?;
            // A frame that decodes SHORT is as much a corruption as one that overruns; without
            // this the tail of the buffer would silently read back as zeros.
            if got != n {
                bail!("decoded length {got} != declared raw {n}");
            }
            Ok(out)
        }
        other => bail!("unknown block codec {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressible_input_uses_zstd_and_roundtrips() {
        let raw = "the shared system prompt, repeated. ".repeat(64).into_bytes();
        let (codec, payload) = encode(&raw, None, LEVEL_DEFAULT).unwrap();
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
        let (codec, payload) = encode(&raw, None, LEVEL_DEFAULT).unwrap();
        assert_eq!(codec, CODEC_STORED, "must fall back rather than store an expanded payload");
        assert_eq!(payload.len(), raw.len(), "stored <= raw is structural");
        assert_eq!(decode(codec, &payload, raw.len() as u32, None).unwrap(), raw);
    }

    #[test]
    fn empty_piece_roundtrips() {
        let raw: Vec<u8> = Vec::new();
        let (codec, payload) = encode(&raw, None, LEVEL_DEFAULT).unwrap();
        assert_eq!(codec, CODEC_STORED);
        assert_eq!(decode(codec, &payload, 0, None).unwrap(), raw);
    }

    #[test]
    fn dictionary_roundtrips() {
        let dict = "gen_ai.request.model gen_ai.usage tool_use assistant ".repeat(16).into_bytes();
        let raw = b"{\"gen_ai.request.model\":\"claude\",\"tool_use\":true}".to_vec();
        let (codec, payload) = encode(&raw, Some(&dict), LEVEL_DEFAULT).unwrap();
        // whichever codec won, it must decode byte-exact with the dictionary available
        assert_eq!(decode(codec, &payload, raw.len() as u32, Some(&dict)).unwrap(), raw);
    }

    #[test]
    fn wrong_declared_length_refuses() {
        let raw = b"abcdefghij".to_vec();
        let (codec, payload) = encode(&raw, None, LEVEL_DEFAULT).unwrap();
        assert!(decode(codec, &payload, 9, None).is_err(), "length mismatch must fail loud");
    }
}
