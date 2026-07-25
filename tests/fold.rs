//! Step-1 gate: the fold stores content once and returns it byte-exact, and survives a torn tail.
//!
//! These are the properties everything above the fold rests on. If any of them fails, nothing built
//! on top can be correct.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use turndb::fold::{Fold, FoldCfg, FoldTail};
use turndb::PieceHash;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-fold-{tag}-{}-{n}", std::process::id()))
}

/// A corpus with the shapes a trace store actually sees: tiny, large, repeated, binary, empty.
fn corpus() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"x".to_vec(),
        b"{\"role\":\"user\",\"content\":\"hello\"}".to_vec(),
        "a long shared system prompt. ".repeat(200).into_bytes(),
        (0u8..=255).collect(),
    ];
    // pseudo-random but deterministic blobs of varied size
    let mut seed = [7u8; 32];
    for i in 0..64 {
        let mut b = Vec::new();
        let target = 1 + (i * 137) % 9000;
        while b.len() < target {
            seed = blake3::hash(&seed).into();
            b.extend_from_slice(&seed);
        }
        b.truncate(target);
        v.push(b);
    }
    // JSON-ish text that compresses well
    for i in 0..16 {
        v.push(format!("{{\"gen_ai.request.model\":\"claude-{i}\",\"tokens\":{}}}", i * 31).repeat(20).into_bytes());
    }
    v
}

#[test]
fn round_trip_is_byte_exact() {
    let dir = tmp("roundtrip");
    let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
    let data = corpus();
    let mut locs = Vec::new();
    for d in &data {
        let p = f.put(d).unwrap();
        assert_eq!(p.hash, PieceHash::of(d), "hash must be BLAKE3 of the exact bytes");
        locs.push(p);
    }
    for (d, p) in data.iter().zip(&locs) {
        assert_eq!(&f.read(p.loc).unwrap(), d, "byte-exact reconstruction failed");
        assert_eq!(&f.read_verified(p.loc, p.hash).unwrap(), d);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn identical_content_is_stored_once() {
    let dir = tmp("dedup");
    let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
    let blob = "the shared prompt every session repeats. ".repeat(50).into_bytes();

    let first = f.put(&blob).unwrap();
    assert!(!first.deduped);
    f.sync().unwrap(); // seal the block so the measurement is against durable bytes
    let after_first = f.disk_bytes();

    for _ in 0..50 {
        let again = f.put(&blob).unwrap();
        assert!(again.deduped, "identical content must not be appended twice");
        assert_eq!(again.loc, first.loc, "a duplicate must resolve to the original location");
    }
    f.sync().unwrap();
    assert_eq!(f.disk_bytes(), after_first, "dedup must write zero additional bytes");
    assert_eq!(f.read(first.loc).unwrap(), blob);

    // distinct content that merely starts the same is NOT deduped
    let mut other = blob.clone();
    other.push(b'!');
    let o = f.put(&other).unwrap();
    assert!(!o.deduped);
    assert_ne!(o.loc, first.loc);
    assert_eq!(f.read(o.loc).unwrap(), other);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reopen_recovers_and_reads_everything() {
    let dir = tmp("reopen");
    let data = corpus();
    let mut locs = Vec::new();
    {
        let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
        for d in &data {
            locs.push(f.put(d).unwrap().loc);
        }
        f.sync().unwrap();
    }
    let f = Fold::open(&dir, FoldCfg::default()).unwrap();
    for (d, l) in data.iter().zip(&locs) {
        assert_eq!(&f.read(*l).unwrap(), d, "content lost or moved across reopen");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn appends_continue_correctly_after_reopen() {
    let dir = tmp("continue");
    let a = b"first generation".to_vec();
    let b = b"second generation".to_vec();
    let la = {
        let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
        let l = f.put(&a).unwrap().loc;
        f.sync().unwrap();
        l
    };
    let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
    let lb = f.put(&b).unwrap().loc;
    assert!(lb.block_id > la.block_id, "the new piece must land in a later block than the recovered tail");
    assert_eq!(f.read(la).unwrap(), a);
    assert_eq!(f.read(lb).unwrap(), b);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn torn_tail_is_truncated_and_good_data_survives() {
    let dir = tmp("torn");
    let data = corpus();
    let mut locs = Vec::new();
    {
        let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
        for d in &data {
            locs.push(f.put(d).unwrap().loc);
        }
        f.sync().unwrap();
    }
    let good_len = std::fs::metadata(dir.join("seg-00000000.fold")).unwrap().len();

    // Simulate a crash mid-append: a frame header promising a payload that never fully landed.
    {
        let mut seg = OpenOptions::new().append(true).open(dir.join("seg-00000000.fold")).unwrap();
        let mut torn = vec![0xA5u8, 1];
        torn.extend_from_slice(&9999u32.to_le_bytes()); // raw
        torn.extend_from_slice(&5000u32.to_le_bytes()); // stored — but we write far less
        torn.extend_from_slice(&[0xAB, 0xCD]);
        torn.extend_from_slice(&[0u8; 64]);
        seg.write_all(&torn).unwrap();
        seg.sync_all().unwrap();
    }
    assert!(std::fs::metadata(dir.join("seg-00000000.fold")).unwrap().len() > good_len);

    let f = Fold::open(&dir, FoldCfg::default()).unwrap();
    assert_eq!(
        std::fs::metadata(dir.join("seg-00000000.fold")).unwrap().len(),
        good_len,
        "recovery must truncate back to the last complete, checksum-valid frame"
    );
    for (d, l) in data.iter().zip(&locs) {
        assert_eq!(&f.read(*l).unwrap(), d, "good data must survive a torn tail");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn corrupted_frame_is_refused_not_served() {
    let dir = tmp("corrupt");
    let blob = "content that must never come back wrong. ".repeat(20).into_bytes();
    let loc = {
        let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
        let l = f.put(&blob).unwrap().loc;
        f.sync().unwrap();
        l
    };
    // flip a byte inside the payload
    {
        use std::os::unix::fs::FileExt;
        let seg = OpenOptions::new().read(true).write(true).open(dir.join("seg-00000000.fold")).unwrap();
        // first block: segment header (48) + block header (16), then into the payload
        let _ = loc;
        let at = 48u64 + 16 + 4;
        let mut b = [0u8; 1];
        seg.read_exact_at(&mut b, at).unwrap();
        b[0] ^= 0xFF;
        seg.write_all_at(&b, at).unwrap();
        seg.sync_all().unwrap();
    }
    let f = Fold::open(&dir, FoldCfg::default()).unwrap();
    assert!(f.read(loc).is_err(), "corrupt content must fail loud, never be served silently");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn segments_roll_and_locs_resolve_across_them() {
    let dir = tmp("roll");
    // tiny blocks AND tiny segments so a modest corpus seals many blocks across several segments
    let cfg = FoldCfg { seg_max: 256 * 1024, block_target: 32 * 1024, ..Default::default() };
    let mut f = Fold::open(&dir, cfg).unwrap();
    let mut data = Vec::new();
    let mut locs = Vec::new();
    let mut seed = [3u8; 32];
    for i in 0..300 {
        let mut b = Vec::new();
        while b.len() < 500 + (i % 7) * 300 {
            seed = blake3::hash(&seed).into();
            b.extend_from_slice(&seed);
        }
        let p = f.put(&b).unwrap();
        locs.push(p.loc);
        data.push(b);
    }
    // Blocks are compressed off the write path and land on completion, so segment state is not
    // final until the pipeline drains.
    f.sync().unwrap();
    assert!(f.segment_count() > 1, "the corpus must have rolled at least one segment");
    let blocks_used: std::collections::HashSet<u32> = locs.iter().map(|l| l.block_id).collect();
    assert!(blocks_used.len() > 1, "the corpus must span multiple blocks");
    drop(f);

    let f = Fold::open(&dir, cfg).unwrap();
    for (d, l) in data.iter().zip(&locs) {
        assert_eq!(&f.read(*l).unwrap(), d, "cross-segment read failed");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_piece_larger_than_seg_max_gets_its_own_segment() {
    let dir = tmp("bigpiece");
    let mut f = Fold::open(&dir, FoldCfg { seg_max: 4096, ..Default::default() }).unwrap();
    let small = b"small".to_vec();
    let s = f.put(&small).unwrap();
    let mut big = Vec::new();
    let mut seed = [9u8; 32];
    while big.len() < 40_000 {
        seed = blake3::hash(&seed).into();
        big.extend_from_slice(&seed);
    }
    let b = f.put(&big).unwrap();
    f.sync().unwrap();
    assert_eq!(f.read(s.loc).unwrap(), small);
    assert_eq!(f.read(b.loc).unwrap(), big, "an oversized piece must still round-trip");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn second_writer_is_refused() {
    let dir = tmp("lock");
    let _f = Fold::open(&dir, FoldCfg::default()).unwrap();
    assert!(
        Fold::open(&dir, FoldCfg::default()).is_err(),
        "the single-writer invariant must be enforced, not merely documented"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn committed_tail_beyond_the_last_good_frame_refuses() {
    let dir = tmp("liar");
    {
        let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
        f.put(b"a little content").unwrap();
        f.sync().unwrap();
    }
    // A commit authority claiming durability past what the fold actually holds means the disk broke a
    // promise. Serving that store would silently lose data.
    let bogus = FoldTail { seg: 0, off: 10_000_000 };
    assert!(Fold::open_at(&dir, FoldCfg::default(), Some(bogus)).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn committed_tail_discards_uncommitted_frames() {
    let dir = tmp("rollback");
    let keep = b"committed content".to_vec();
    let (loc, tail) = {
        let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
        let l = f.put(&keep).unwrap().loc;
        let t = f.sync().unwrap();
        // written after the commit point — a crash before the next commit must roll these away
        f.put(b"uncommitted one").unwrap();
        f.put(b"uncommitted two").unwrap();
        (l, t)
    };
    let f = Fold::open_at(&dir, FoldCfg::default(), Some(tail)).unwrap();
    assert_eq!(f.read(loc).unwrap(), keep);
    assert_eq!(f.tail(), tail, "the fold must resume exactly at the committed tail");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dedup_window_seals_without_affecting_reads() {
    let dir = tmp("window");
    let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
    let blob = b"content spanning a seal".to_vec();
    let p = f.put(&blob).unwrap();
    assert_eq!(f.window_len(), 1);
    f.seal_window();
    assert_eq!(f.window_len(), 0);
    // the window is an accelerator, not a source of truth: reads are unaffected
    assert_eq!(f.read(p.loc).unwrap(), blob);
    // and re-putting after a seal is merely a duplicate append, never wrong
    let again = f.put(&blob).unwrap();
    assert_eq!(f.read(again.loc).unwrap(), blob);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pieces_written_together_share_a_block() {
    // The locality that makes blocking pay: a record's pieces are captured together, so they land in
    // one block and reconstructing the record costs one decompression, not one per piece.
    let dir = tmp("locality");
    let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
    let mut locs = Vec::new();
    for i in 0..40 {
        locs.push(f.put(format!("{{\"role\":\"user\",\"content\":\"message {i}\"}}").as_bytes()).unwrap().loc);
    }
    let blocks: std::collections::HashSet<u32> = locs.iter().map(|l| l.block_id).collect();
    assert_eq!(blocks.len(), 1, "40 small pieces written together must share one block");
    // and their in-block offsets are strictly increasing — they were laid down in order
    for w in locs.windows(2) {
        assert!(w[1].in_off > w[0].in_off);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn block_cache_serves_neighbours() {
    let dir = tmp("cache");
    let cfg = FoldCfg { block_target: 4096, ..Default::default() };
    let mut f = Fold::open(&dir, cfg).unwrap();
    let mut locs = Vec::new();
    for i in 0..200 {
        locs.push(f.put(format!("piece number {i} with some padding to give it size").as_bytes()).unwrap().loc);
    }
    f.sync().unwrap();
    drop(f);

    let f = Fold::open(&dir, cfg).unwrap();
    for l in &locs {
        f.read(*l).unwrap();
    }
    let s = f.cache_stats();
    assert!(s.misses >= 1, "the first read of a block must miss");
    assert!(
        s.hits > s.misses * 10,
        "neighbours must come from cache: {} hits vs {} misses",
        s.hits, s.misses
    );
    std::fs::remove_dir_all(&dir).ok();
}
