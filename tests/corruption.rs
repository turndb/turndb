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
use turndb::types::{AttrValue, BodyOp, Content, Record, BODY_CONTENT};

fn body_content(ops: Vec<BodyOp>) -> Vec<Content> {
    let identity = match ops.as_slice() {
        [BodyOp::Piece { hash, .. }] => turndb::ContentHash(hash.0),
        ops if ops.iter().all(|op| matches!(op, BodyOp::Lit(_))) => {
            let bytes = ops
                .iter()
                .flat_map(|op| match op {
                    BodyOp::Lit(bytes) => bytes.as_slice(),
                    BodyOp::Piece { .. } => unreachable!(),
                })
                .copied()
                .collect::<Vec<_>>();
            turndb::ContentHash::of(&bytes)
        }
        _ => return vec![Content::new(BODY_CONTENT, ops)],
    };
    vec![Content::identified(BODY_CONTENT, ops, identity)]
}

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

/// STORM_XOR varies every seed for soak runs — unset, it is 0 and the run is the deterministic
/// default. Every mutation loop routes through this, hand-rolled ones included: a soak that only
/// reaches some of the mutants is a soak that silently re-treads the rest. A finding reports the
/// EFFECTIVE seed, so a discovery replays exactly by exporting that value.
fn seeded(base: u64) -> u64 {
    base ^ std::env::var("STORM_XOR").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
}

/// Run `f` against `rounds` mutants of `pristine`, writing each to `path` first. `f` may error all
/// it likes; a panic is the failure, reported with the seed and round that reproduce it.
fn storm(tag: &str, pristine: &[u8], path: &Path, rounds: usize, seed: u64, f: impl Fn(&Path)) {
    let seed = seeded(seed);
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
            contents: vec![Content::identified(
                BODY_CONTENT,
                vec![
                    BodyOp::Lit(b"[".to_vec()),
                    BodyOp::Piece { hash: p.hash, len: p.loc.raw },
                    BodyOp::Lit(b"]".to_vec()),
                ],
                turndb::ContentHash::of(format!("[{body}]").as_bytes()),
            )],
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
                contents: body_content(vec![BodyOp::Piece { hash: h, len: bytes.len() as u32 }]),
                attrs: vec![
                    ("k".into(), AttrValue::Str("v".into())),
                    ("f".into(), AttrValue::Float(-0.0)),
                ],
            };
            w.append(i, &r, &[(h, bytes)]).unwrap();
        }
        w.append_tomb(10, "w:3").unwrap();
        let extra = Record {
            id: "b:1".into(),
            contents: body_content(vec![BodyOp::Lit(b"lit".to_vec())]),
            attrs: vec![],
        };
        w.append_batch(11, &[(extra, Vec::new(), false)]).unwrap();
        w.sync().unwrap();
    }
    let pristine = std::fs::read(&path).unwrap();
    let target = dir.join("WAL.mutant");
    storm("wal", &pristine, &target, 8000, 0xD4A1, |p| {
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
        contents: vec![Content::identified(
            BODY_CONTENT,
            vec![
                BodyOp::Lit(b"[".to_vec()),
                BodyOp::Piece { hash: h, len: body_bytes.len() as u32 },
            ],
            turndb::ContentHash::of(&[b"[".as_slice(), body_bytes.as_slice()].concat()),
        )],
        attrs: vec![
            ("s".into(), AttrValue::Str("v".into())),
            ("i".into(), AttrValue::Int(-9)),
            ("f".into(), AttrValue::Float(f64::NAN)),
            ("b".into(), AttrValue::Bool(true)),
        ],
    };
    let mut pristine = Vec::new();
    turndb::store::wal::encode_record(&mut pristine, &r, &[(h, body_bytes)]).unwrap();

    let seed = seeded(0xC0DEC);
    let mut rng = Rng(seed);
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
        let mut fold =
            Fold::open(&src, FoldCfg { block_target: 4096, ..Default::default() }).unwrap();
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

/// The container's two hand-rolled parsers: the superblock decode and the varint directory walk.
///
/// Both were written for this format and neither had been fuzzed — the storm's own premise is that
/// hand-rolled parsing over bytes from a broken disk is exactly where panics hide, and a directory
/// walk that reads a length and then a slice is the shape that hides them best.
#[test]
fn container_parsers_never_panic_on_damage() {
    let dir = tmp("container");
    std::fs::create_dir_all(&dir).unwrap();

    // A container with several members, so the directory carries real entries to damage: a
    // one-member container would never exercise the walk past its first iteration.
    let built = dir.join("built.turndb");
    let mut s =
        Store::open_file(&built, FoldCfg { block_target: 4096, ..Default::default() }).unwrap();
    for i in 0..12 {
        let body = vec![b'a' + (i % 26) as u8; 900];
        s.put(&format!("c:{i:02}"), &[Span::Piece(&body)], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();
    let pristine = std::fs::read(&built).unwrap();
    assert!(pristine.len() > 8192, "the fixture must have members past the superblocks");

    let target = dir.join("mutant.turndb");
    storm("container", &pristine, &target, 6000, 0xC0117A, |p| {
        let Ok(c) = turndb::container::Container::open(p) else { return };
        // An open that survives a mutant must still fail closed on every other surface: a
        // directory that parsed is not a directory that points anywhere real.
        let _ = c.verify();
        let _ = c.free_bytes();
        let _ = c.member_bytes();
        for name in c.names().map(String::from).collect::<Vec<_>>() {
            let _ = c.read_file_bounded(&name, 1 << 20);
            let _ = c.extent(&name);
        }
        // And the whole store on top of it, which is where a plausible-but-wrong directory does
        // its damage: an extent that parses as a part but names the wrong bytes.
        let Ok(rs) = turndb::store::open_read_container(p, FoldCfg::default()) else { return };
        let _ = rs.ids();
        for i in 0..12 {
            let _ = rs.reconstruct(&format!("c:{i:02}"));
        }
    });
    std::fs::remove_dir_all(&dir).ok();
}

/// The directory walk, reached through its checksum rather than around it.
///
/// The byte storm above cannot get here. Every route to this parser is gated: a flip in the
/// superblock fails its BLAKE3, a flip in the directory payload fails its crc32, so the varint
/// walk never sees damaged bytes and a missing bounds check inside it survives six thousand
/// mutants untouched. That is defence in depth working exactly as intended and a blind spot in the
/// test at the same time.
///
/// A checksum proves bytes did not drift; it proves nothing about what they mean. A writer bug, or
/// anyone who can run this format's own encoder, produces a directory that passes every checksum
/// and still claims a name longer than the payload or an extent past the end of the file. So this
/// mutates the DECOMPRESSED payload and then repairs both checksums over it, which is the only way
/// the walk is reachable at all.
#[test]
fn a_checksum_valid_directory_with_hostile_contents_is_refused_not_trusted() {
    let dir = tmp("container-dir");
    std::fs::create_dir_all(&dir).unwrap();

    let built = dir.join("built.turndb");
    let mut s =
        Store::open_file(&built, FoldCfg { block_target: 4096, ..Default::default() }).unwrap();
    for i in 0..8 {
        let body = vec![b'x'; 700];
        s.put(&format!("d:{i:02}"), &[Span::Piece(&body)], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();
    let pristine = std::fs::read(&built).unwrap();

    // The live slot is the one with the higher sequence; both carry the magic.
    let slot_len = turndb::container::SLOT_LEN as usize;
    let seq_at = |b: &[u8], at: usize| u64::from_le_bytes(b[at + 8..at + 16].try_into().unwrap());
    let live = if seq_at(&pristine, slot_len) > seq_at(&pristine, 0) { slot_len } else { 0 };
    let get64 = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    let get32 = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let dir_off = get64(&pristine, live + 16) as usize;
    let dir_stored = get32(&pristine, live + 24) as usize;
    let dir_raw = get32(&pristine, live + 28);
    let codec = pristine[live + 48];
    let payload =
        turndb::fold::codec::decode(codec, &pristine[dir_off..dir_off + dir_stored], dir_raw, None)
            .unwrap();
    assert!(payload.len() > 32, "the fixture directory must have entries to damage");

    let target = dir.join("hostile.turndb");
    let seed = seeded(0xD142EC);
    let mut rng = Rng(seed);
    let mut opened = 0usize;
    for round in 0..4000 {
        let mut mutated = payload.clone();
        for _ in 0..rng.below(3) + 1 {
            mutate(&mut mutated, &mut rng);
        }
        // Rebuild the container around the damaged payload, stored rather than compressed, with
        // both checksums recomputed so nothing refuses it before the walk runs.
        let mut file = pristine[..dir_off].to_vec();
        let new_off = file.len() as u64;
        file.extend_from_slice(&mutated);
        let tail = file.len() as u64;

        let mut slot = [0u8; 4096];
        slot.copy_from_slice(&pristine[live..live + slot_len]);
        slot[16..24].copy_from_slice(&new_off.to_le_bytes());
        slot[24..28].copy_from_slice(&(mutated.len() as u32).to_le_bytes());
        slot[28..32].copy_from_slice(&(mutated.len() as u32).to_le_bytes());
        slot[36..40].copy_from_slice(&crc32fast::hash(&mutated).to_le_bytes());
        slot[40..48].copy_from_slice(&tail.to_le_bytes());
        slot[48] = 0; // stored
        let digest = blake3::hash(&slot[0..52]);
        slot[52..56].copy_from_slice(&digest.as_bytes()[0..4]);

        // Both slots carry it, so the reader cannot fall back to an undamaged older state.
        file[0..slot_len].copy_from_slice(&slot);
        file[slot_len..slot_len * 2].copy_from_slice(&slot);
        std::fs::write(&target, &file).unwrap();

        let r = catch_unwind(AssertUnwindSafe(|| {
            let Ok(c) = turndb::container::Container::open(&target) else { return false };
            let _ = c.verify();
            for name in c.names().map(String::from).collect::<Vec<_>>() {
                let _ = c.read_file_bounded(&name, 1 << 20);
            }
            let _ = turndb::store::open_read_container(&target, FoldCfg::default());
            true
        }));
        let survived = match r {
            Ok(v) => v,
            Err(_) => panic!(
                "container directory: PANIC on a checksum-valid hostile directory (seed {seed}, round \
                 {round}) — a parser must refuse, not panic"
            ),
        };
        if survived {
            opened += 1;
        }
    }
    // If nothing ever parsed, the harness is checking that the checksum works rather than that the
    // walk does, and the next missing bounds check goes unnoticed exactly as the last one did.
    assert!(opened > 0, "no mutated directory ever parsed: the walk is still not being reached");
    println!("container directory: 4000 hostile directories, {opened} parsed and were survived");
    std::fs::remove_dir_all(&dir).ok();
}
