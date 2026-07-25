//! Model-based verification: randomized lifecycles against a reference that is correct by construction.
//!
//! # Why not a differential gate against the old engine
//!
//! The plan once called for checking this rebuild against its predecessor. That predecessor carries the
//! design this rebuild exists to escape, so its behaviour is not the specification — when two
//! implementations disagree you learn that they disagree, not which is right. It would also couple the
//! rebuild to a repo we deliberately left behind: kept buildable, kept API-compatible, and silently
//! useless the moment either format moved.
//!
//! For a content-addressed store none of that is needed, because **the ground truth is the input**. What
//! went in must come out, byte for byte. That is checkable directly and it is a stronger claim than
//! agreement with another implementation, which can be wrong in the same way.
//!
//! So the oracle here is a `BTreeMap` — correct by construction, owned outright, no dependency at all.
//!
//! # What is actually being tested
//!
//! Not "does a put round-trip" — the other suites cover that. This drives randomized sequences of
//! put / sync / flush / merge / compact / reopen / **crash** and checks the durability contract after
//! every step:
//!
//! - a record that was **synced** MUST survive a crash, byte-exact
//! - a record that was **not** synced may vanish — but if it is there, it must be correct
//! - reads never return content that was never written, whatever the sequence
//!
//! That second rule is the one worth stating: a test that demanded unsynced writes survive would be
//! testing a promise the store never made, and a test that demanded they vanish would forbid a legal
//! implementation. The contract is one-sided and the model encodes exactly that.

use std::collections::BTreeMap;
use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::store::{Span, Store};
use turndb::AttrValue;

/// Deterministic PRNG. A failing seed reproduces exactly, which is the only reason a randomized test
/// is worth having.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

type Attrs = Vec<(String, AttrValue)>;

/// The reference. A map from id to the last thing written under it — nothing more.
#[derive(Default, Clone)]
struct Model {
    /// Durable: written and synced. These MUST survive anything.
    acked: BTreeMap<String, (Vec<u8>, Attrs)>,
    /// Written but not yet synced. These MAY be lost by a crash, and must be correct if present.
    staged: BTreeMap<String, (Vec<u8>, Attrs)>,
}

impl Model {
    fn put(&mut self, id: &str, body: Vec<u8>, attrs: Attrs) {
        self.staged.insert(id.to_string(), (body, attrs));
    }
    fn sync(&mut self) {
        for (k, v) in std::mem::take(&mut self.staged) {
            self.acked.insert(k, v);
        }
    }
    /// Reconcile with reality after a crash, checking legality on the way.
    ///
    /// Dropping the store is a clean process exit, not a power cut: WAL bytes reach the page cache, so
    /// unsynced writes MAY survive. A model that demanded they vanish would forbid a legal
    /// implementation, and one that demanded they survive would test a promise never made. So the
    /// contract checked here is exactly the one-sided one —
    ///
    ///   * a SYNCED record must still be there,
    ///   * an unsynced record may be there or not, but if it is, it must hold either the value it was
    ///     last written with or the last value that was synced — never anything else.
    ///
    /// Once checked, whatever survived becomes the model's truth: the ambiguity is resolved by
    /// observation, not by guessing.
    fn reconcile(&mut self, s: &Store, ctx: &str) {
        for (id, staged) in std::mem::take(&mut self.staged) {
            let acked = self.acked.get(&id).cloned();
            match s.get(&id).unwrap_or_else(|e| panic!("{ctx}: get({id}) errored: {e}")) {
                Some(rec) => {
                    let body = s
                        .reconstruct(&id)
                        .unwrap_or_else(|e| panic!("{ctx}: reconstruct({id}) errored: {e}"))
                        .expect("get found it, so reconstruct must too");
                    let is_staged = body == staged.0 && rec.attrs == staged.1;
                    let is_acked = acked.as_ref().is_some_and(|a| body == a.0 && rec.attrs == a.1);
                    assert!(
                        is_staged || is_acked,
                        "{ctx}: after a crash {id} holds a value that was never written for it"
                    );
                    if is_staged {
                        self.acked.insert(id, staged);
                    }
                }
                None => assert!(
                    acked.is_none(),
                    "{ctx}: a SYNCED record ({id}) vanished across a crash"
                ),
            }
        }
    }
}

fn cfg() -> FoldCfg {
    FoldCfg { seg_max: 1 << 20, block_target: 1 << 14, ..FoldCfg::default() }
}

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-model-{tag}-{}-{n}", std::process::id()))
}

/// A pool of reusable content, so dedup is genuinely exercised: most writes repeat earlier content,
/// which is the regime this engine is built for and the one where Tier-0/Tier-1 interact.
fn body_for(r: &mut Rng, pool: &[Vec<u8>]) -> Vec<u8> {
    let n = 1 + r.below(4);
    let mut out = Vec::new();
    for _ in 0..n {
        out.extend_from_slice(&pool[r.below(pool.len())]);
    }
    out
}

fn attrs_for(r: &mut Rng) -> Attrs {
    let mut a = Vec::new();
    if r.below(10) > 0 {
        a.push(("kind".into(), AttrValue::Str(["req", "resp", "tool"][r.below(3)].into())));
    }
    if r.below(10) > 2 {
        a.push(("n".into(), AttrValue::Int(r.below(1000) as i64)));
    }
    if r.below(10) > 6 {
        a.push(("ratio".into(), AttrValue::Float(r.below(100) as f64 / 7.0)));
    }
    if r.below(10) > 7 {
        a.push(("ok".into(), AttrValue::Bool(r.below(2) == 0)));
    }
    a
}

/// Every id the model knows must read back exactly, and nothing else may appear.
fn verify(s: &Store, m: &Model, ctx: &str) {
    // The EFFECTIVE view: a staged write shadows the acked one. Chaining the two maps would check an
    // id twice and assert its superseded value on the first pass.
    let mut eff: BTreeMap<&String, &(Vec<u8>, Attrs)> = m.acked.iter().collect();
    for (k, v) in &m.staged {
        eff.insert(k, v);
    }
    for (id, (body, attrs)) in eff {
        let got = s
            .reconstruct(id)
            .unwrap_or_else(|e| panic!("{ctx}: reconstruct({id}) errored: {e}"));
        let got = got.unwrap_or_else(|| panic!("{ctx}: {id} is missing"));
        assert_eq!(&got, body, "{ctx}: {id} reconstructed to the wrong bytes");

        let rec = s.get(id).unwrap().unwrap_or_else(|| panic!("{ctx}: get({id}) is missing"));
        assert_eq!(&rec.attrs, attrs, "{ctx}: {id} attributes diverged");
    }
    // No id the model never wrote may exist.
    let live: Vec<String> = s.ids().unwrap();
    for id in &live {
        assert!(
            m.acked.contains_key(id) || m.staged.contains_key(id),
            "{ctx}: store holds {id}, which was never written"
        );
    }
}

/// One randomized lifecycle. Returns the number of operations applied.
fn run_seed(seed: u64, steps: usize) -> usize {
    let mut r = Rng(seed.max(1));
    let dir = tmp(&format!("s{seed}"));
    let pool: Vec<Vec<u8>> = (0..24)
        .map(|i| {
            let mut v = Vec::new();
            for j in 0..(8 + (i * 37) % 400) {
                v.extend_from_slice(blake3::hash(&((i * 1000 + j) as u32).to_le_bytes()).as_bytes());
            }
            v
        })
        .collect();

    let mut m = Model::default();
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut applied = 0usize;

    for step in 0..steps {
        let ctx = format!("seed {seed} step {step}");
        match r.below(100) {
            // put — ids repeat, so version resolution across parts is exercised
            0..=54 => {
                let id = format!("id{:03}", r.below(60));
                let body = body_for(&mut r, &pool);
                let attrs = attrs_for(&mut r);
                s.put(&id, &[Span::Piece(&body)], attrs.clone()).unwrap();
                m.put(&id, body, attrs);
            }
            // put with mixed literal + piece spans
            55..=64 => {
                let id = format!("id{:03}", r.below(60));
                let lit = format!("<{}>", r.below(10000)).into_bytes();
                let piece = body_for(&mut r, &pool);
                let attrs = attrs_for(&mut r);
                s.put(&id, &[Span::Lit(&lit), Span::Piece(&piece)], attrs.clone()).unwrap();
                let mut body = lit.clone();
                body.extend_from_slice(&piece);
                m.put(&id, body, attrs);
            }
            65..=74 => {
                s.sync().unwrap();
                m.sync();
            }
            75..=84 => {
                s.sync().unwrap();
                m.sync();
                s.flush().unwrap();
            }
            85..=89 => {
                // merge a random valid contiguous run
                let n = s.part_count();
                if n >= 2 {
                    let len = 2 + r.below(n - 1);
                    let len = len.min(n);
                    let lo = r.below(n - len + 1);
                    s.merge_range(lo, len).unwrap();
                }
            }
            90..=93 => {
                s.maybe_compact(4, 3).unwrap();
            }
            // clean reopen: sync first, so nothing may be lost
            94..=96 => {
                s.sync().unwrap();
                m.sync();
                drop(s);
                s = Store::open(&dir, cfg()).unwrap();
            }
            // CRASH: drop with no sync.
            _ => {
                drop(s);
                s = Store::open(&dir, cfg()).unwrap();
                m.reconcile(&s, &ctx);
            }
        }
        applied += 1;
        verify(&s, &m, &ctx);
    }

    // and it must survive one more round trip through the disk
    s.sync().unwrap();
    m.sync();
    s.flush().unwrap();
    drop(s);
    let s = Store::open(&dir, cfg()).unwrap();
    verify(&s, &m, &format!("seed {seed} final reopen"));

    // a reader with no lock sees the same committed state
    let rs = Store::open_read(&dir, cfg()).unwrap();
    for (id, (body, _)) in &m.acked {
        assert_eq!(
            &rs.reconstruct(id).unwrap().unwrap(),
            body,
            "seed {seed}: a lockless reader diverged on {id}"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
    applied
}

/// Scale knob. `TURNDB_SOAK=20` multiplies both seed count and depth — the same test, run long, is
/// how a randomized suite earns trust without slowing every `cargo test`.
fn soak() -> usize {
    std::env::var("TURNDB_SOAK").ok().and_then(|v| v.parse().ok()).unwrap_or(1usize).max(1)
}

#[test]
fn randomized_lifecycles_preserve_every_written_byte() {
    let k = soak();
    let mut total = 0usize;
    for seed in 1..=(12 * k) as u64 {
        total += run_seed(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 120 * k);
    }
    assert!(total >= 1440);
}

#[test]
fn a_long_lifecycle_stays_consistent() {
    // One deep sequence rather than many shallow ones: merges only become interesting once parts have
    // accumulated, and version resolution only bites once an id has been rewritten across several.
    run_seed(0xDEAD_BEEF, 900 * soak());
}

#[test]
fn crash_never_resurrects_and_never_corrupts() {
    // The contract is one-sided and this pins both sides of it: synced writes must survive, unsynced
    // writes may vanish but must never come back WRONG.
    let dir = tmp("crashcontract");
    let mut r = Rng(7);
    let mut m = Model::default();
    let mut s = Store::open(&dir, cfg()).unwrap();
    let pool: Vec<Vec<u8>> = (0..8)
        .map(|i| (0..200).flat_map(|j| blake3::hash(&((i * 500 + j) as u32).to_le_bytes()).as_bytes()[..16].to_vec()).collect())
        .collect();

    for round in 0..40 {
        for k in 0..5 {
            let id = format!("d{:02}", (round * 5 + k) % 30);
            let body = body_for(&mut r, &pool);
            s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
            m.put(&id, body, vec![]);
        }
        if round % 3 == 0 {
            s.sync().unwrap();
            m.sync();
        }
        if round % 5 == 0 {
            s.flush().unwrap();
        }
        // hard stop, no sync
        drop(s);
        s = Store::open(&dir, cfg()).unwrap();
        m.reconcile(&s, &format!("round {round}"));

        for (id, (body, _)) in &m.acked {
            assert_eq!(
                &s.reconstruct(id).unwrap().unwrap(),
                body,
                "round {round}: a SYNCED record did not survive the crash"
            );
        }
        // nothing may exist that was never written
        for id in s.ids().unwrap() {
            assert!(m.acked.contains_key(&id), "round {round}: store holds {id}, never written");
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}
