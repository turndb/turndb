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
    let Some(bytes) = usize::try_from(m.div_ceil(8)).ok().and_then(|n| n.checked_add(8)) else {
        return false;
    };
    if m == 0 || sec.len() != bytes {
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
    pub fn encoded_len_for_capacity(n: usize) -> Result<usize> {
        let count = u64::try_from(n).map_err(|_| anyhow::anyhow!("piece count exceeds u64"))?;
        let m = count
            .max(1)
            .checked_mul(BITS_PER)
            .ok_or_else(|| anyhow::anyhow!("bloom bit count overflows"))?
            .max(64);
        usize::try_from(m.div_ceil(8))
            .ok()
            .and_then(|bytes| bytes.checked_add(8))
            .ok_or_else(|| anyhow::anyhow!("bloom encoding exceeds this process's address space"))
    }

    pub fn try_with_capacity(n: usize) -> Result<Bloom> {
        let encoded = Self::encoded_len_for_capacity(n)?;
        let bytes = encoded - 8;
        let count = u64::try_from(n).map_err(|_| anyhow::anyhow!("piece count exceeds u64"))?;
        let m = count
            .max(1)
            .checked_mul(BITS_PER)
            .ok_or_else(|| anyhow::anyhow!("bloom bit count overflows"))?
            .max(64);
        let mut bits = Vec::new();
        bits.try_reserve_exact(bytes).map_err(|error| anyhow::anyhow!(error))?;
        bits.resize(bytes, 0);
        Ok(Bloom { bits, m })
    }

    pub fn with_capacity(n: usize) -> Bloom {
        Self::try_with_capacity(n).expect("Bloom capacity must be representable")
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
        self.try_encode().expect("Bloom encoding capacity was admitted at construction")
    }

    pub fn try_encode(&self) -> Result<Vec<u8>> {
        let capacity = self
            .bits
            .len()
            .checked_add(8)
            .ok_or_else(|| anyhow::anyhow!("bloom encoding length overflows"))?;
        let mut out = Vec::new();
        out.try_reserve_exact(capacity).map_err(|error| anyhow::anyhow!(error))?;
        out.extend_from_slice(&self.m.to_le_bytes());
        out.extend_from_slice(&self.bits);
        Ok(out)
    }

    pub fn decode(b: &[u8]) -> Result<Bloom> {
        if b.len() < 8 {
            bail!("bloom section truncated");
        }
        let m = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let bytes = usize::try_from(m.div_ceil(8))
            .ok()
            .and_then(|n| n.checked_add(8))
            .ok_or_else(|| anyhow::anyhow!("bloom bit count exceeds this platform"))?;
        if m == 0 || b.len() != bytes {
            bail!("bloom bit array does not match its declared size");
        }
        Ok(Bloom { m, bits: b[8..].to_vec() })
    }

    /// Validate the exact current-format filter parameters and prove that every authoritative
    /// piece identity remains a possible hit. Extra set bits are harmless false positives; a
    /// missing required bit would let an advisory structure change a logical dedup answer.
    pub fn validate_current(b: &[u8], hashes: &[u8]) -> Result<()> {
        if !hashes.len().is_multiple_of(32) {
            bail!("piece hash column ends with a partial identity");
        }
        let filter = Bloom::decode(b)?;
        let pieces = u64::try_from(hashes.len() / 32)
            .map_err(|_| anyhow::anyhow!("piece count exceeds u64"))?;
        let expected_m = pieces
            .max(1)
            .checked_mul(BITS_PER)
            .ok_or_else(|| anyhow::anyhow!("bloom bit count overflows"))?
            .max(64);
        if filter.m != expected_m {
            bail!(
                "bloom bit count {} does not match the current-format value {expected_m}",
                filter.m
            );
        }
        for encoded in hashes.chunks_exact(32) {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(encoded);
            if !filter.maybe_contains(&PieceHash(hash)) {
                bail!("bloom filter has a false negative for a declared piece identity");
            }
        }
        Ok(())
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
    fn trailing_bytes_are_outside_the_current_bloom_grammar() {
        let mut encoded = Bloom::with_capacity(1).encode();
        encoded.push(0);
        assert!(Bloom::decode(&encoded).is_err());
        assert!(!probe_encoded(&encoded, &PieceHash::of(b"piece")));
    }

    #[test]
    fn empty_is_valid_and_says_no() {
        let b = Bloom::with_capacity(0);
        assert!(!b.maybe_contains(&PieceHash::of(b"anything")));
        assert!(Bloom::decode(&b.encode()).is_ok());
    }
}
