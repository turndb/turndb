//! The corruption storm: every parser must REFUSE damaged input — an error, never a panic, never
//! an abort, never a wild allocation. These are the surfaces where bytes from a broken disk (or an
//! adversarial file) meet hand-rolled parsing: the part footer/TOC walk, the WAL replay loop, the
//! fold tail scan, and the manifest.
//!
//! Deterministic on purpose: a seeded generator, so a failure is a seed to replay rather than a
//! lost coincidence. This is the fuzzing harness's stand-in until a libFuzzer setup exists — same
//! oracle (don't panic), reproducible corpus, runs under plain `cargo test`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use turndb::fold::{Fold, FoldCfg};
use turndb::part::{self, Part};
use turndb::store::wal::Wal;
use turndb::store::{Span, Store};
use turndb::types::{AttrValue, BodyOp, Record};

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-storm-{tag}-{}-{n}", std::process::id()))
}

/// xorshift64* — tiny, seedable, good enough to scatter mutations.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One of the damage shapes a disk or a tool actually produces: bit rot, splatted runs, zeroed
/// runs, truncation, and trailing garbage.
fn mutate(bytes: &mut Vec<u8>, rng: &mut Rng) {
    if bytes.is_empty() {
        bytes.extend((0..rng.below(64)).map(|_| rng.next() as u8));
        return;
    }
    match rng.below(6) {
        0 => {
            let i = rng.below(bytes.len());
            bytes[i] ^= 1 << rng.below(8);
        }
        1 => {
            let i = rng.below(bytes.len());
            bytes[i] = rng.next() as u8;
        }
        2 | 3 => {
            let i = rng.below(bytes.len());
            let n = rng.below(32) + 1;
            let v = if rng.below(2) == 0 { 0x00 } else { 0xFF };
            for b in bytes.iter_mut().skip(i).take(n) {
                *b = v;
            }
        }
        4 => {
            bytes.truncate(rng.below(bytes.len()));
        }
        _ => {
            let n = rng.below(64) + 1;
            bytes.extend((0..n).map(|_| rng.next() as u8));
        }
    }
}

/// Run `f` against `rounds` mutants of `pristine`, writing each to `path` first. `f` may error all
/// it likes; a panic is the failure, reported with the seed and round that reproduce it.
fn storm(
    tag: &str,
    pristine: &[u8],
    path: &Path,
    rounds: usize,
    seed: u64,
    f: impl Fn(&Path),
) {
    // STORM_XOR varies every storm's seed for soak runs — unset, it is 0 and the run is the
    // deterministic default. A finding reports the EFFECTIVE seed, so a soak discovery replays
    // exactly by exporting that value.
    let seed = seed ^ std::env::var("STORM_XOR").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let mut rng = Rng(seed);
    for round in 0..rounds {
        let mut m = pristine.to_vec();
        // one to three stacked mutations — single flips find the checksum misses, stacks find the
        // parser slips
        for _ in 0..rng.below(3) + 1 {
            mutate(&mut m, &mut rng);
        }
        std::fs::write(path, &m).unwrap();
        let r = catch_unwind(AssertUnwindSafe(|| f(path)));
        assert!(
            r.is_ok(),
            "{tag}: PANIC on mutant (seed {seed}, round {round}) — a parser must refuse, not panic"
        );
    }
}

/// A real part with pieces, attributes of every type, and a tombstone — every section present.
fn build_part(dir: &Path) -> (Vec<u8>, Fold) {
    let mut fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();
    let mut records = Vec::new();
    let mut tombs = Vec::new();
    for i in 0..20 {
        let body = format!("piece body {i}, padded to be worth folding out {}", "x".repeat(i * 7));
        let p = fold.put(body.as_bytes()).unwrap();
        records.push(Record {
            id: format!("rec:{i:03}"),
            body: vec![
                BodyOp::Lit(b"[".to_vec()),
                BodyOp::Piece { hash: p.hash, len: p.loc.raw },
                BodyOp::Lit(b"]".to_vec()),
            ],
            attrs: vec![
                ("model".into(), AttrValue::Str(format!("m{}", i % 3))),
                ("n".into(), AttrValue::Int(i as i64)),
                ("f".into(), AttrValue::Float(i as f64 * 0.5)),
                ("ok".into(), AttrValue::Bool(i % 2 == 0)),
            ],
        });
        tombs.push(i == 7);
    }
    let path = dir.join("p.part");
    part::build_full(&path, &records, &tombs, 1, 1, 3, |h| fold.lookup(*h), &Default::default())
        .unwrap();
    fold.sync().unwrap();
    (std::fs::read(&path).unwrap(), fold)
}

#[test]
fn part_parsers_never_panic_on_damage() {
    let dir = tmp("part");
    std::fs::create_dir_all(&dir).unwrap();
    let (pristine, fold) = build_part(&dir);
    let target = dir.join("mutant.part");
    let probe = turndb::PieceHash::of(b"probe");
    storm("part", &pristine, &target, 6000, 0xDEC0DE, |p| {
        let Ok(part) = Part::open(p) else { return };
        // An open that SUCCEEDS on a mutant must still fail closed everywhere else: walk every
        // read surface and let errors happen — they just must be errors.
        let _ = part.verify_sections();
        let _ = part.ids();
        let _ = part.find("rec:005");
        let _ = part.tombstones();
        let _ = part.lookup_piece(&probe);
        for c in 0..8 {
            let _ = part.zone(c);
        }
        let n = part.len().min(64);
        for r in 0..n {
            let _ = part.body(r);
            let _ = part.attrs(r);
            let _ = part.record(r);
            let _ = part.reconstruct(r, &fold);
        }
    });
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn wal_replay_never_panics_on_damage() {
    let dir = tmp("wal");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("WAL");
    {
        let mut w = Wal::open(&path).unwrap();
        for i in 0..10u64 {
            let body = format!("wal piece {i} {}", "y".repeat(i as usize * 11));
            let bytes = body.into_bytes();
            let h = turndb::PieceHash::of(&bytes);
            let r = Record {
                id: format!("w:{i}"),
                body: vec![BodyOp::Piece { hash: h, len: bytes.len() as u32 }],
                attrs: vec![
                    ("k".into(), AttrValue::Str("v".into())),
                    ("f".into(), AttrValue::Float(-0.0)),
                ],
            };
            w.append(i, &r, &[(h, bytes)]).unwrap();
        }
        w.append_tomb(10, "w:3").unwrap();
        let extra = Record { id: "b:1".into(), body: vec![BodyOp::Lit(b"lit".to_vec())], attrs: vec![] };
        w.append_batch(11, &[(extra, Vec::new(), false)]).unwrap();
        w.sync().unwrap();
    }
    let pristine = std::fs::read(&path).unwrap();
    let target = dir.join("WAL.mutant");
    storm("wal", &pristine, &target, 8000, 0x57A1, |p| {
        let _ = Wal::replay(p);
    });
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn wal_record_decode_never_panics_on_damage() {
    // decode_record normally sits behind the crc, but the crc detects torn writes, not writer
    // bugs — a buggy writer checksums its bugs perfectly. Storm the decoder DIRECTLY, no crc
    // shield in front of it.
    let body_bytes = b"decode storm piece".to_vec();
    let h = turndb::PieceHash::of(&body_bytes);
    let r = Record {
        id: "d:1".into(),
        body: vec![
            BodyOp::Lit(b"[".to_vec()),
            BodyOp::Piece { hash: h, len: body_bytes.len() as u32 },
        ],
        attrs: vec![
            ("s".into(), AttrValue::Str("v".into())),
            ("i".into(), AttrValue::Int(-9)),
            ("f".into(), AttrValue::Float(f64::NAN)),
            ("b".into(), AttrValue::Bool(true)),
        ],
    };
    let mut pristine = Vec::new();
    turndb::store::wal::encode_record(&mut pristine, &r, &[(h, body_bytes)]);

    let mut rng = Rng(0xC0DEC);
    for round in 0..15000 {
        let mut m = pristine.clone();
        for _ in 0..rng.below(3) + 1 {
            mutate(&mut m, &mut rng);
        }
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = turndb::store::wal::decode_record(&m);
        }));
        assert!(res.is_ok(), "decode_record: PANIC on mutant (round {round})");
    }
}

#[test]
fn fold_open_never_panics_on_damage() {
    let dir = tmp("fold");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("fold");
    let mut locs = Vec::new();
    {
        let mut fold = Fold::open(&src, FoldCfg { block_target: 4096, ..Default::default() }).unwrap();
        for i in 0..50 {
            let body = format!("fold piece {i} {}", "z".repeat(i * 13));
            let p = fold.put(body.as_bytes()).unwrap();
            locs.push((p.loc, p.hash));
        }
        fold.sync().unwrap();
    }
    let seg = src.join("seg-00000000.fold");
    let pristine = std::fs::read(&seg).unwrap();
    // Mutants overwrite the ONE segment in place; open_read rescans it from scratch each time.
    storm("fold", &pristine, &seg, 4000, 0xF01D, |_| {
        let Ok(f) = Fold::open_read(&src, FoldCfg::default()) else { return };
        for (loc, hash) in locs.iter().take(16) {
            let _ = f.read_verified(*loc, *hash);
        }
    });
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pack_open_never_panics_on_damage() {
    let dir = tmp("pack");
    std::fs::create_dir_all(&dir).unwrap();
    let store_dir = dir.join("store");
    {
        let mut s = Store::open(&store_dir, FoldCfg::default()).unwrap();
        for i in 0..8 {
            s.put(
                &format!("p:{i}"),
                &[Span::Piece(format!("pack storm body {i} {}", "q".repeat(i * 31)).as_bytes())],
                vec![("n".into(), AttrValue::Int(i as i64))],
            )
            .unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let pk = dir.join("p.turndb");
    turndb::pack::write(&store_dir, &pk).unwrap();
    let pristine = std::fs::read(&pk).unwrap();
    let target = dir.join("mutant.turndb");
    storm("pack", &pristine, &target, 2500, 0x9AC4, |p| {
        // the full read stack over a damaged pack: footer, TOC, manifest, fold, parts
        let Ok(pack) = turndb::pack::Pack::open(p) else { return };
        let _ = pack.verify();
        let Ok(rs) = turndb::store::open_read_pack(p, FoldCfg::default()) else { return };
        let _ = rs.ids();
        let _ = rs.reconstruct("p:3");
    });
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn store_open_never_panics_on_manifest_damage() {
    let dir = tmp("manifest");
    std::fs::create_dir_all(&dir).unwrap();
    {
        let mut s = Store::open(&dir, FoldCfg::default()).unwrap();
        s.put("k", &[Span::Piece(b"manifest storm body, long enough to fold")], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let man = dir.join("MANIFEST");
    let pristine = std::fs::read(&man).unwrap();
    // Mutate MANIFEST in place and open READ-ONLY: a reader must refuse damage without panicking —
    // and without mutating anything, which is why the writer's open is not the probe here.
    storm("manifest", &pristine, &man, 3000, 0x3A21F&0xFFFF, |_| {
        let _ = Store::open_read(&dir, FoldCfg::default());
    });
    std::fs::remove_dir_all(&dir).ok();
}
