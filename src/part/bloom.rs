//! A Bloom filter over a part's piece hashes — the first rung of cross-part dedup.
//!
//! # Why a filter at all
//!
//! Dedup across parts means asking every part "have you seen this content?". Without a filter that is
//! a disk touch per part per piece, and at 97% duplication almost every write is such a question. The
//! filter answers "definitely not" from memory for nearly all of them.
//!
//! # The asymmetry that makes it safe
//!
//! A Bloom filter has **no false negatives**, so it can never hide a piece we have already stored —
//! it costs a missed dedup opportunity exactly never. A false positive costs one wasted lookup, which
//! the sorted hash column then rejects definitively. This is the same rule the in-memory window
//! follows: allowed to be lossy, never allowed to be wrong.
//!
//! ~10 bits per piece gives roughly 1% false positives — about 1.25 MB per million pieces, so a
//! 60 TB store's filters are ~1 GB resident.

use crate::types::PieceHash;
use anyhow::{bail, Result};

const BITS_PER: u64 = 10;
const K: u32 = 7; // ~0.693 * BITS_PER, the optimum for this bit budget

pub struct Bloom {
    bits: Vec<u8>,
    m: u64,
}

/// BLAKE3 output is already uniformly distributed, so probe positions are sliced straight out of it —
/// double hashing over two of its words, with no additional hash function.
#[inline]
fn probe(h: &PieceHash, i: u64, m: u64) -> u64 {
    let a = u64::from_le_bytes(h.0[0..8].try_into().unwrap());
    let b = u64::from_le_bytes(h.0[8..16].try_into().unwrap()) | 1;
    a.wrapping_add(i.wrapping_mul(b)) % m
}

/// Probe the *encoded* section directly, skipping a decode and copy on the hot path — the part already
/// caches sections in decompressed form, so this is pure arithmetic over borrowed bytes.
///
/// A malformed section answers `false`, which by the no-false-negatives contract can only cost a missed
/// dedup, never a wrong answer.
pub fn probe_encoded(sec: &[u8], h: &PieceHash) -> bool {
    if sec.len() < 8 {
        return false;
    }
    let m = u64::from_le_bytes(sec[0..8].try_into().unwrap());
    if m == 0 {
        return false;
    }
    let bits = &sec[8..];
    (0..K as u64).all(|i| {
        let p = probe(h, i, m);
        let byte = (p / 8) as usize;
        byte < bits.len() && bits[byte] & (1 << (p % 8)) != 0
    })
}

impl Bloom {
    pub fn with_capacity(n: usize) -> Bloom {
        let m = ((n as u64).max(1) * BITS_PER).max(64);
        Bloom { bits: vec![0u8; m.div_ceil(8) as usize], m }
    }

    pub fn insert(&mut self, h: &PieceHash) {
        for i in 0..K as u64 {
            let p = probe(h, i, self.m);
            self.bits[(p / 8) as usize] |= 1 << (p % 8);
        }
    }

    /// `false` is definitive: this part does not hold the piece. `true` means "look it up".
    pub fn maybe_contains(&self, h: &PieceHash) -> bool {
        (0..K as u64).all(|i| {
            let p = probe(h, i, self.m);
            self.bits[(p / 8) as usize] & (1 << (p % 8)) != 0
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.bits.len());
        out.extend_from_slice(&self.m.to_le_bytes());
        out.extend_from_slice(&self.bits);
        out
    }

    pub fn decode(b: &[u8]) -> Result<Bloom> {
        if b.len() < 8 {
            bail!("bloom section truncated");
        }
        let m = u64::from_le_bytes(b[0..8].try_into().unwrap());
        if m == 0 || b.len() - 8 < m.div_ceil(8) as usize {
            bail!("bloom bit array does not match its declared size");
        }
        Ok(Bloom { m, bits: b[8..].to_vec() })
    }

    pub fn bytes(&self) -> usize {
        self.bits.len() + 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_gives_a_false_negative() {
        // The one property that matters: a stored piece must never be reported absent.
        let mut b = Bloom::with_capacity(5000);
        let present: Vec<PieceHash> =
            (0..5000).map(|i| PieceHash::of(format!("p{i}").as_bytes())).collect();
        for h in &present {
            b.insert(h);
        }
        for h in &present {
            assert!(b.maybe_contains(h), "FALSE NEGATIVE — dedup would silently lose a hit");
        }
    }

    #[test]
    fn false_positive_rate_is_near_the_budget() {
        let mut b = Bloom::with_capacity(10_000);
        for i in 0..10_000 {
            b.insert(&PieceHash::of(format!("in{i}").as_bytes()));
        }
        let fp = (0..20_000)
            .filter(|i| b.maybe_contains(&PieceHash::of(format!("out{i}").as_bytes())))
            .count();
        let rate = fp as f64 / 20_000.0;
        assert!(rate < 0.05, "false positive rate {rate:.4} far above the ~1% budget");
    }

    #[test]
    fn roundtrips_through_its_section_bytes() {
        let mut b = Bloom::with_capacity(500);
        let hs: Vec<PieceHash> =
            (0..500).map(|i| PieceHash::of(format!("r{i}").as_bytes())).collect();
        for h in &hs {
            b.insert(h);
        }
        let back = Bloom::decode(&b.encode()).unwrap();
        for h in &hs {
            assert!(back.maybe_contains(h));
        }
        assert!(Bloom::decode(&[0u8; 4]).is_err(), "a truncated section must refuse");
    }

    #[test]
    fn empty_is_valid_and_says_no() {
        let b = Bloom::with_capacity(0);
        assert!(!b.maybe_contains(&PieceHash::of(b"anything")));
        assert!(Bloom::decode(&b.encode()).is_ok());
    }
}
