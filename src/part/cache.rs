//! A byte-budgeted LRU shared by every open part.
//!
//! Parts cached several decoded views without limit: decompressed sections, decoded offset arrays,
//! row indices, and dictionaries. Measured on a real store, a part pins **9.5x its on-disk size**
//! once fully read — 19.5 MiB for a 2 MiB part — so a hundred-part store costs about 2 GiB to read,
//! and nothing ever released it.
//!
//! # Why not simply cache less
//!
//! Every one of those caches exists because removing it made a whole-part walk quadratic: merge was
//! measured at 493s over 8x50k records before the offset arrays and dictionaries were held. So the
//! answer cannot be "cache less", it has to be "cache the same things, bounded, and evict the coldest".
//!
//! # Where the floor actually is (measured, and not where it was assumed)
//!
//! A merge interleaves reads across all of its input parts — the k-way walk advances whichever part
//! holds the next id — so the obvious guess is that the budget must hold every input's hot sections at
//! once. Measured over a merge of 8 parts (~19.5 MiB hot each, ~156 MiB together), that guess is
//! wrong:
//!
//! ```text
//!   512 MiB   29.6s          64 MiB   39.6s   +34%
//!   256 MiB   29.6s          32 MiB   45.3s   +53%
//!   128 MiB   33.3s   +12%   16 MiB   >200s   COLLAPSED
//! ```
//!
//! Undersizing degrades GRADUALLY down to about a fifth of the all-parts working set, then falls off a
//! cliff. The cliff is not at the sum — it is where the budget drops below ONE part's hot sections,
//! because past that point a part cannot hold its own working set and every read evicts the entry it
//! just made. LRU is the worst possible policy for a cyclic access pattern, and a k-way merge is
//! exactly cyclic.
//!
//! So the rule is: **the budget must comfortably exceed a single part's hot set**, and more is better
//! but sharply diminishing. The default is 512 MiB, roughly 26 parts' worth here, which leaves the
//! cliff far out of reach.
//!
//! Eviction only drops the cache's own handle. A caller holding an `Arc` keeps its data valid, so
//! eviction can never invalidate a read in progress.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::ContentMeta;

/// Default budget across all parts sharing one cache. See the module note on why this is not small.
pub const BUDGET_DEFAULT: usize = 512 << 20;

/// Distinguishes the four caches within one part, so their keys cannot collide.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    /// A decompressed section, by name.
    Section(String),
    /// A decoded fixed-width array, by section name.
    Nums(String),
    /// A decoded row-index array, by column ordinal.
    Rids(usize),
    /// A decoded named-content row-index array, by content-column ordinal.
    ContentRids(usize),
    /// The decoded named-content column directory.
    ContentMeta,
    /// A decoded string dictionary, by column ordinal.
    Dict(usize),
    /// A decoded binary dictionary, by column ordinal.
    BinaryDict(usize),
    /// The decoded id column. Front-coded on disk, so every read of it reconstructs prefixes and
    /// validates UTF-8 — worth doing once.
    Ids,
}

#[derive(Clone)]
pub enum Held {
    Bytes(Arc<Vec<u8>>),
    Nums(Arc<Vec<u64>>),
    Rids(Arc<Vec<u32>>),
    Strings(Arc<Vec<String>>),
    ByteStrings(Arc<Vec<Vec<u8>>>),
    ContentMeta(Arc<Vec<ContentMeta>>),
}

impl Held {
    fn weight(&self) -> usize {
        match self {
            Held::Bytes(v) => v.len(),
            Held::Nums(v) => v.len() * 8,
            Held::Rids(v) => v.len() * 4,
            // A String is a pointer, length and capacity beyond its bytes; ignoring that would
            // under-count a dictionary of short strings several times over.
            Held::Strings(v) => v.iter().map(|s| s.len() + std::mem::size_of::<String>()).sum(),
            Held::ByteStrings(v) => {
                v.iter().map(|s| s.len() + std::mem::size_of::<Vec<u8>>()).sum()
            }
            Held::ContentMeta(v) => {
                v.iter().map(|m| m.name.len() + std::mem::size_of::<ContentMeta>()).sum()
            }
        }
    }
}

struct Inner {
    bytes: usize,
    clock: u64,
    map: HashMap<(u64, Kind), (u64, Held)>,
}

pub struct SectionCache {
    budget: usize,
    inner: Mutex<Inner>,
}

/// Identity for a part within a shared cache. Monotonic, so a reopened part never inherits stale
/// entries from a closed one.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_part_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

impl SectionCache {
    pub fn new(budget: usize) -> SectionCache {
        SectionCache {
            budget: budget.max(1 << 20),
            inner: Mutex::new(Inner { bytes: 0, clock: 0, map: HashMap::new() }),
        }
    }

    /// A fresh budget, for a caller that wants its parts accounted separately (a `Store` does).
    pub fn shared() -> Arc<SectionCache> {
        Arc::new(SectionCache::new(BUDGET_DEFAULT))
    }

    /// The process-wide default, used by [`super::Part::open`].
    ///
    /// A per-part default would defeat the purpose: N parts opened standalone would carry N budgets
    /// and grow without bound in exactly the way this exists to prevent. Anything that opens parts
    /// without saying otherwise draws on one budget.
    pub fn global() -> Arc<SectionCache> {
        static G: std::sync::OnceLock<Arc<SectionCache>> = std::sync::OnceLock::new();
        G.get_or_init(|| Arc::new(SectionCache::new(BUDGET_DEFAULT))).clone()
    }

    pub fn get(&self, part: u64, k: &Kind) -> Option<Held> {
        let mut g = self.inner.lock().unwrap();
        g.clock += 1;
        let c = g.clock;
        let e = g.map.get_mut(&(part, k.clone()))?;
        e.0 = c;
        Some(e.1.clone())
    }

    /// Insert, evicting the coldest entries until the budget holds again.
    ///
    /// One entry is always admitted however large it is — a section bigger than the whole budget must
    /// still be readable, and refusing it would make the part unusable rather than merely slow.
    pub fn put(&self, part: u64, k: Kind, v: Held) -> Held {
        let w = v.weight();
        let mut g = self.inner.lock().unwrap();
        while g.bytes + w > self.budget && !g.map.is_empty() {
            let victim = g.map.iter().min_by_key(|(_, (t, _))| *t).map(|(key, _)| key.clone());
            let Some(victim) = victim else { break };
            if let Some((_, gone)) = g.map.remove(&victim) {
                g.bytes -= gone.weight();
            }
        }
        g.clock += 1;
        let c = g.clock;
        g.bytes += w;
        if let Some((_, old)) = g.map.insert((part, k), (c, v.clone())) {
            g.bytes -= old.weight();
        }
        v
    }

    /// Drop everything belonging to one part. Called when a `Part` is dropped, so a closed part does
    /// not hold budget hostage until it happens to be evicted.
    pub fn forget(&self, part: u64) {
        let mut g = self.inner.lock().unwrap();
        let keys: Vec<(u64, Kind)> = g.map.keys().filter(|(p, _)| *p == part).cloned().collect();
        for k in keys {
            if let Some((_, gone)) = g.map.remove(&k) {
                g.bytes -= gone.weight();
            }
        }
    }

    pub fn bytes(&self) -> usize {
        self.inner.lock().unwrap().bytes
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn entries(&self) -> usize {
        self.inner.lock().unwrap().map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(n: usize) -> Held {
        Held::Bytes(Arc::new(vec![0u8; n]))
    }

    #[test]
    fn stays_inside_its_budget() {
        let c = SectionCache::new(4 << 20);
        for i in 0..64 {
            c.put(1, Kind::Section(format!("s{i}")), bytes(1 << 20));
            assert!(c.bytes() <= 4 << 20, "budget exceeded at insert {i}: {}", c.bytes());
        }
    }

    #[test]
    fn an_oversized_entry_is_still_admitted() {
        // Refusing it would make a part with one huge section unreadable rather than slow.
        let c = SectionCache::new(1 << 20);
        c.put(1, Kind::Section("big".into()), bytes(8 << 20));
        assert!(c.get(1, &Kind::Section("big".into())).is_some());
    }

    #[test]
    fn re_inserting_a_key_does_not_leak_budget() {
        let c = SectionCache::new(64 << 20);
        for _ in 0..50 {
            c.put(7, Kind::Rids(3), Held::Rids(Arc::new(vec![0u32; 1 << 18])));
        }
        assert_eq!(c.entries(), 1);
        assert_eq!(c.bytes(), (1 << 18) * 4, "a displaced value must stop being counted");
    }

    #[test]
    fn forgetting_a_part_releases_exactly_its_own_bytes() {
        let c = SectionCache::new(64 << 20);
        c.put(1, Kind::Section("a".into()), bytes(1 << 20));
        c.put(2, Kind::Section("a".into()), bytes(1 << 20));
        let both = c.bytes();
        c.forget(1);
        assert_eq!(c.bytes(), both / 2);
        assert!(c.get(1, &Kind::Section("a".into())).is_none());
        assert!(c.get(2, &Kind::Section("a".into())).is_some(), "the other part is untouched");
    }

    #[test]
    fn eviction_never_invalidates_a_live_handle() {
        let c = SectionCache::new(2 << 20);
        let held = c.put(1, Kind::Section("keep".into()), bytes(1 << 20));
        for i in 0..16 {
            c.put(1, Kind::Section(format!("push{i}")), bytes(1 << 20));
        }
        assert!(c.get(1, &Kind::Section("keep".into())).is_none(), "it should have been evicted");
        // ...but the handle taken before eviction is still perfectly good
        match held {
            Held::Bytes(v) => assert_eq!(v.len(), 1 << 20),
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn part_ids_are_unique() {
        let a = next_part_id();
        let b = next_part_id();
        assert_ne!(a, b);
    }
}
