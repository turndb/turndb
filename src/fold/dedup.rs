//! The dedup accelerator: content hash → location, for pieces not yet sealed into a part.
//!
//! # The asymmetry that dictates the design
//!
//! | index state | consequence |
//! |---|---|
//! | complete | every duplicate piece is stored once |
//! | cold / partial | duplicate bytes appended — the fold is *larger*, every read still byte-exact |
//! | **false positive** | **catastrophic** — a distinct piece aliased to the wrong bytes |
//!
//! So the index is allowed to be lossy and is never allowed to be wrong. Truncated hashes may filter;
//! only a full 32-byte compare may conclude a hit.
//!
//! # Why it has no on-disk form
//!
//! There is no fact only this index knows. Pieces already sealed into parts are found through those
//! parts' own dictionaries; pieces not yet sealed are replayed from the WAL, which carries the carved
//! result. Nothing to persist, corrupt, recover, or keep consistent.

use super::block::Loc;
use crate::types::PieceHash;

const EMPTY: [u8; 32] = [0u8; 32];

#[derive(Clone, Copy)]
struct Slot {
    hash: [u8; 32],
    loc: Loc,
}

/// Open-addressed, linear-probe table. 48 bytes per live entry, capacity a power of two, load factor
/// held at or below 0.75.
pub struct DedupTable {
    slots: Vec<Slot>,
    mask: usize,
    len: usize,
    /// The all-zero hash doubles as the empty marker, so the (astronomically unlikely) real piece
    /// hashing to it is held aside rather than corrupting the probe.
    zero: Option<Loc>,
}

impl DedupTable {
    pub fn new() -> DedupTable {
        DedupTable::with_capacity(1 << 12)
    }

    pub fn with_capacity(cap_pow2: usize) -> DedupTable {
        let cap = cap_pow2.next_power_of_two().max(16);
        DedupTable {
            slots: vec![Slot { hash: EMPTY, loc: Loc::default() }; cap],
            mask: cap - 1,
            len: 0,
            zero: None,
        }
    }

    pub fn len(&self) -> usize {
        self.len + usize::from(self.zero.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Approximate resident bytes — for flush-trigger accounting.
    pub fn heap_bytes(&self) -> usize {
        self.slots.len() * std::mem::size_of::<Slot>()
    }

    /// BLAKE3 output is already uniformly distributed, so its first 8 bytes ARE the probe hash.
    /// Running a second hash function over it would be pure waste.
    #[inline]
    fn probe_start(&self, h: &[u8; 32]) -> usize {
        (u64::from_le_bytes(h[0..8].try_into().unwrap()) as usize) & self.mask
    }

    pub fn get(&self, hash: &PieceHash) -> Option<Loc> {
        if hash.0 == EMPTY {
            return self.zero;
        }
        let mut i = self.probe_start(&hash.0);
        loop {
            let s = &self.slots[i];
            if s.hash == EMPTY {
                return None;
            }
            // full 32-byte compare — the only thing allowed to conclude equality
            if s.hash == hash.0 {
                return Some(s.loc);
            }
            i = (i + 1) & self.mask;
        }
    }

    pub fn insert(&mut self, hash: PieceHash, loc: Loc) {
        if hash.0 == EMPTY {
            self.zero = Some(loc);
            return;
        }
        if (self.len + 1) * 4 > self.slots.len() * 3 {
            self.grow();
        }
        self.insert_raw(hash.0, loc);
    }

    fn insert_raw(&mut self, hash: [u8; 32], loc: Loc) {
        let mut i = self.probe_start(&hash);
        loop {
            if self.slots[i].hash == EMPTY {
                self.slots[i] = Slot { hash, loc };
                self.len += 1;
                return;
            }
            if self.slots[i].hash == hash {
                return; // already present; first location wins (content is identical either way)
            }
            i = (i + 1) & self.mask;
        }
    }

    fn grow(&mut self) {
        let bigger = vec![Slot { hash: EMPTY, loc: Loc::default() }; self.slots.len() * 2];
        let old = std::mem::replace(&mut self.slots, bigger);
        self.mask = self.slots.len() - 1;
        self.len = 0;
        for s in old {
            if s.hash != EMPTY {
                self.insert_raw(s.hash, s.loc);
            }
        }
    }

    /// Drop every entry — called once the window they cover has been sealed into a part, so resident
    /// memory is bounded by the flush interval rather than by store size.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.hash = EMPTY;
        }
        self.len = 0;
        self.zero = None;
    }
}

impl Default for DedupTable {
    fn default() -> Self {
        DedupTable::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(block_id: u32) -> Loc {
        Loc { block_id, in_off: 0, raw: 20 }
    }

    #[test]
    fn insert_get_and_miss() {
        let mut t = DedupTable::with_capacity(16);
        let a = PieceHash::of(b"alpha");
        let b = PieceHash::of(b"beta");
        assert!(t.get(&a).is_none());
        t.insert(a, loc(48));
        assert_eq!(t.get(&a), Some(loc(48)));
        assert!(t.get(&b).is_none(), "a miss must not alias to another entry");
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn survives_growth_with_no_loss_or_aliasing() {
        let mut t = DedupTable::with_capacity(16);
        let mut expect = Vec::new();
        for i in 0..2000u32 {
            let h = PieceHash::of(format!("piece-{i}").as_bytes());
            t.insert(h, loc(48 + i));
            expect.push((h, loc(48 + i)));
        }
        assert_eq!(t.len(), 2000);
        for (h, l) in &expect {
            assert_eq!(t.get(h), Some(*l), "entry lost or moved across growth");
        }
        // and nothing that was never inserted resolves
        for i in 2000..2100u32 {
            let h = PieceHash::of(format!("piece-{i}").as_bytes());
            assert!(t.get(&h).is_none(), "false positive — the one forbidden failure");
        }
    }

    #[test]
    fn reinsert_keeps_first_location() {
        let mut t = DedupTable::new();
        let h = PieceHash::of(b"same bytes");
        t.insert(h, loc(100));
        t.insert(h, loc(999));
        assert_eq!(t.get(&h), Some(loc(100)));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn all_zero_hash_is_held_aside_not_confused_with_empty() {
        let mut t = DedupTable::with_capacity(16);
        let z = PieceHash([0u8; 32]);
        assert!(t.get(&z).is_none());
        t.insert(z, loc(7));
        assert_eq!(t.get(&z), Some(loc(7)), "the empty marker must not swallow a real entry");
        assert_eq!(t.len(), 1);
        let other = PieceHash::of(b"other");
        assert!(t.get(&other).is_none());
    }

    #[test]
    fn clear_releases_the_window() {
        let mut t = DedupTable::new();
        let h = PieceHash::of(b"x");
        t.insert(h, loc(1));
        t.clear();
        assert!(t.is_empty());
        assert!(t.get(&h).is_none());
    }
}
