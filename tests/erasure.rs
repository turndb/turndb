//! Erasure reports itself as erasure, and says so from the manifest rather than from the bytes.
//!
//! FORMAT.md places two conditions on erasure, the one anticipated exception to byte-exact
//! reconstruction: a reader must be told what it gets instead of the content, and a partially
//! erased record must not become unreadable. This file covers the first.
//!
//! The defect these exist for: `punch_blocks` zeroes a block's PAYLOAD and deliberately leaves its
//! 16-byte header intact so the frame chain stays walkable. The erasure-aware read path tested for
//! an all-zero HEADER, so it could not fire for anything turndb itself punched — and a deliberate
//! erasure was reported as "block checksum mismatch (torn write or corruption)", which is the ops
//! fire drill the punch ordering exists to prevent. Nothing in the bytes distinguishes the two
//! cases; only the manifest does.

use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::store::{Span, Store};

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-erasure-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    // small segments so a block leaves the active segment, which is never punched
    FoldCfg { seg_max: 1 << 20, block_target: 64 * 1024, ..Default::default() }
}

/// Incompressible, so each piece is a real block rather than sharing one.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 64);
    let mut h = blake3::hash(&seed.to_le_bytes());
    while out.len() < len {
        out.extend_from_slice(h.as_bytes());
        h = blake3::hash(h.as_bytes());
    }
    out.truncate(len);
    out
}

/// Supersede a record so its content becomes unreachable from the live snapshot, punch, and leave a
/// retained manifest that still NAMES the record. Returns the store dir and the retained commit.
fn store_with_a_punched_retained_record(tag: &str) -> (PathBuf, u64) {
    let dir = tmp(tag);
    let mut s = Store::open_file(&store_file(&dir), cfg()).unwrap();

    s.put("k", &[Span::Piece(&noise(1, 64 * 1024))], vec![]).unwrap();
    for i in 0..8 {
        s.put(&format!("f{i}"), &[Span::Piece(&noise(100 + i, 64 * 1024))], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    let c1 = s.manifest().commit;

    s.put("k", &[Span::Piece(&noise(2, 1024))], vec![]).unwrap();
    for i in 0..24 {
        s.put(&format!("m{i}"), &[Span::Piece(&noise(200 + i, 64 * 1024))], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();

    let stats = s.punch_unreferenced().unwrap();
    assert!(stats.blocks_punched > 0, "the test needs a real punch: {stats:?}");
    assert!(!s.manifest().punched.is_empty(), "the manifest must name what it punched");
    drop(s);
    (dir, c1)
}

#[test]
fn a_read_of_erased_content_reports_erasure_not_corruption() {
    let (dir, c1) = store_with_a_punched_retained_record("reports");

    let old = turndb::store::open_read_container_at(&store_file(&dir), cfg(), c1).unwrap();
    // The retained snapshot still names the record — only its bytes are gone.
    assert!(old.ids().unwrap().contains(&"k".to_string()));

    let err = old.reconstruct("k").expect_err("erased content must not read back as content");
    let msg = err.to_string();
    assert!(
        msg.contains("ERASED"),
        "a deliberate erasure must name itself; got {msg:?}. Reporting it as corruption sends an \
         operator looking for a failing disk."
    );
    assert!(
        !msg.contains("torn write") && !msg.contains("corruption"),
        "erasure must not be reported as corruption; got {msg:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The live manifest is the ONLY authority for telling erasure from damage, so a retained read must
/// refuse when it cannot be read — never fall back to "nothing was erased".
///
/// Tolerating the load error would rebuild the defect by another route: no declaration, so the
/// punched payload is reported as checksum corruption again. `punched` being absent and `punched`
/// being unknown are different facts, and only one of them is safe to act on.
#[test]
fn an_unreadable_live_manifest_refuses_rather_than_declaring_nothing_erased() {
    let (dir, c1) = store_with_a_punched_retained_record("authority");

    // Corrupt the LIVE manifest member only. The retained one at c1 is untouched, so everything
    // this snapshot names is still held and readable — the only thing missing is the erasure
    // declaration, which is exactly the condition under test.
    let file = store_file(&dir);
    let (m_off, _) = {
        let c = turndb::container::Container::open(&file).unwrap();
        assert!(c.contains(&format!("MANIFEST.{c1:08}")), "the retained manifest must survive");
        c.member_extents("MANIFEST").unwrap()[0]
    };
    let mut bytes = std::fs::read(&file).unwrap();
    bytes[m_off as usize] ^= 0xff;
    std::fs::write(&file, &bytes).unwrap();

    match turndb::store::open_read_container_at(&store_file(&dir), cfg(), c1) {
        Ok(_) => panic!(
            "the live erasure declaration is authoritative and must be readable — opening a \
             retained snapshot without it would report erased blocks as corruption"
        ),
        Err(e) => {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("live manifest"),
                "the refusal must name what could not be read; got {msg:?}"
            );
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// The distinction that makes the message worth anything: erased is not absent.
#[test]
fn erased_is_distinguishable_from_never_existed() {
    let (dir, c1) = store_with_a_punched_retained_record("distinct");
    let old = turndb::store::open_read_container_at(&store_file(&dir), cfg(), c1).unwrap();

    assert!(old.reconstruct("k").is_err(), "erased content refuses");
    assert!(
        old.reconstruct("no-such-id").unwrap().is_none(),
        "an id that never existed is absent, not an error"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Live reads are unaffected by construction — `punch_unreferenced` decides from live visibility, so
/// no live record's blocks are punchable. Asserted rather than assumed: it is the blast radius.
#[test]
fn punching_does_not_disturb_live_reads() {
    let dir = tmp("live");
    let mut s = Store::open_file(&store_file(&dir), cfg()).unwrap();
    let keep = noise(42, 64 * 1024);
    s.put("keeper", &[Span::Piece(&keep)], vec![]).unwrap();
    s.put("victim", &[Span::Piece(&noise(43, 64 * 1024))], vec![]).unwrap();
    for i in 0..24 {
        s.put(&format!("m{i}"), &[Span::Piece(&noise(300 + i, 64 * 1024))], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    s.put("victim", &[Span::Piece(&noise(44, 512))], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    s.punch_unreferenced().unwrap();

    assert_eq!(s.reconstruct("keeper").unwrap().unwrap(), keep, "a live record is byte-exact");
    for i in 0..24 {
        assert!(s.reconstruct(&format!("m{i}")).unwrap().is_some(), "m{i} must still read");
    }
    drop(s);
    std::fs::remove_dir_all(&dir).ok();
}

/// A re-fold rewrites the world WITHOUT the erased content, so the new generation has no holes to
/// declare — and block ids restart per generation, so carrying the old list forward would name live
/// blocks. Inert until something reads `punched`; the moment anything does, a stale range reports
/// live content as erased, which is worse than the defect it was meant to fix.
#[test]
fn a_refold_clears_the_punched_list_it_no_longer_describes() {
    let dir = tmp("refoldclear");
    let mut s = Store::open_file(&store_file(&dir), cfg()).unwrap();
    s.put("k", &[Span::Piece(&noise(11, 64 * 1024))], vec![]).unwrap();
    for i in 0..8 {
        s.put(&format!("f{i}"), &[Span::Piece(&noise(400 + i, 64 * 1024))], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    s.put("k", &[Span::Piece(&noise(12, 1024))], vec![]).unwrap();
    for i in 0..24 {
        s.put(&format!("m{i}"), &[Span::Piece(&noise(500 + i, 64 * 1024))], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();

    s.punch_unreferenced().unwrap();
    let gen_before = s.manifest().fold_gen;
    assert!(!s.manifest().punched.is_empty(), "the test needs a punch to have happened");

    s.refold().unwrap();
    assert_ne!(s.manifest().fold_gen, gen_before, "a re-fold must advance the generation");
    assert!(
        s.manifest().punched.is_empty(),
        "punched ranges name blocks in the generation they were punched from; block ids restart \
         per generation, so carrying them into a new one names LIVE blocks as erased"
    );

    // and the rewritten generation reads back completely
    for i in 0..24 {
        assert!(s.reconstruct(&format!("m{i}")).unwrap().is_some(), "m{i} survived the re-fold");
    }
    drop(s);
    std::fs::remove_dir_all(&dir).ok();
}

/// `erase_ids` destroys ADDRESSABILITY, and must therefore stay silent — the opposite of punching.
///
/// Anything that distinguishes "erased" from "never existed" is itself a residue: it discloses that
/// a record existed, at that id, at that time, which for a compliance erasure is the fact you were
/// required to destroy. The two mechanisms need opposite answers, and that is a property rather
/// than an inconsistency: punching leaves the record addressable, so a reader can still ask and
/// must be told; a re-fold leaves nobody to tell.
#[test]
fn erase_ids_leaves_nothing_to_distinguish_it_from_absence() {
    let dir = tmp("eraseids");
    let mut s = Store::open_file(&store_file(&dir), cfg()).unwrap();
    s.put("aaa-kept", &[Span::Piece(&noise(1, 4096))], vec![]).unwrap();
    s.put("mmm-victim", &[Span::Piece(&noise(2, 4096))], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    let c1 = s.manifest().commit;

    let stats = s.erase_ids(&["mmm-victim".to_string()]).unwrap();
    assert_eq!(stats.tombstoned, 1);

    assert!(s.reconstruct("mmm-victim").unwrap().is_none(), "the erased record is gone");
    assert!(s.reconstruct("mmm-neverwas").unwrap().is_none(), "so is one that never existed");
    assert!(!s.ids().unwrap().contains(&"mmm-victim".to_string()), "the id itself is gone");
    assert!(s.scan_ids(Some("mmm"), Some("mmn"), 10, false).unwrap().is_empty());
    assert_eq!(s.reconstruct("aaa-kept").unwrap().unwrap(), noise(1, 4096), "neighbour intact");

    // And time travel does not quietly serve the erased snapshot: the retained log is purged, so
    // the reader is refused rather than handed a window in which the record still exists.
    assert!(
        turndb::store::open_read_container_at(&store_file(&dir), cfg(), c1).is_err(),
        "a snapshot that could still serve the erased record is not erasure"
    );
    drop(s);
    std::fs::remove_dir_all(&dir).ok();
}

/// A partially-erased record still refuses whole today, even when one of its pieces survives and is
/// independently readable. This pins FORMAT.md's declared condition-2 gap until the public API can
/// return surviving bytes together with exact erased ranges without weakening byte-exact
/// `reconstruct`.
#[test]
fn a_partially_erased_record_refuses_even_though_its_shared_piece_survives() {
    let dir = tmp("partial");
    let mut s = Store::open_file(&store_file(&dir), cfg()).unwrap();
    let shared = noise(900, 64 * 1024);
    let unique = noise(901, 64 * 1024);

    s.put("keeper", &[Span::Piece(&shared)], vec![]).unwrap();
    s.put("victim", &[Span::Piece(&shared), Span::Piece(&unique)], vec![]).unwrap();
    for i in 0..8 {
        s.put(&format!("f{i}"), &[Span::Piece(&noise(920 + i, 64 * 1024))], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    let c1 = s.manifest().commit;

    s.put("victim", &[Span::Piece(&noise(950, 1024))], vec![]).unwrap();
    for i in 0..24 {
        s.put(&format!("m{i}"), &[Span::Piece(&noise(1000 + i, 64 * 1024))], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    assert!(s.punch_unreferenced().unwrap().blocks_punched > 0);
    drop(s);

    let old = turndb::store::open_read_container_at(&store_file(&dir), cfg(), c1).unwrap();
    assert_eq!(
        old.reconstruct("keeper").unwrap().unwrap(),
        shared,
        "the shared piece survives and remains independently readable"
    );
    let err =
        old.reconstruct("victim").expect_err("one erased piece still refuses the whole record");
    assert!(err.to_string().contains("ERASED"), "{err:#}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The migrated suites build single-file stores inside their temp directories: the parent is
/// ensured, the store is one file within it, and every cleanup keeps operating on the directory.
fn store_file(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).ok();
    dir.join("s.turndb")
}
