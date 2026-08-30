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
        v.push(
            format!("{{\"gen_ai.request.model\":\"claude-{i}\",\"tokens\":{}}}", i * 31)
                .repeat(20)
                .into_bytes(),
        );
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
    assert!(
        lb.block_id > la.block_id,
        "the new piece must land in a later block than the recovered tail"
    );
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
        let seg =
            OpenOptions::new().read(true).write(true).open(dir.join("seg-00000000.fold")).unwrap();
        // first block: segment header (48) + block header (16), then into the payload
        let _ = loc;
        let at = 48u64 + 16 + 4;
        let mut b = [0u8; 1];
        read_at(&seg, &mut b, at).unwrap();
        b[0] ^= 0xFF;
        write_at(&seg, &b, at).unwrap();
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
    let error = match Fold::open(&dir, FoldCfg::default()) {
        Ok(_) => panic!("the single-writer invariant must be enforced, not merely documented"),
        Err(error) => error,
    };
    let locked = error
        .downcast_ref::<turndb::fold::WriterLocked>()
        .expect("contention must be typed so bindings do not parse prose");
    assert_eq!(locked.path, dir);
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
    assert!(Fold::open_at(&dir, FoldCfg::default(), Some(bogus), &[]).is_err());
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
    let f = Fold::open_at(&dir, FoldCfg::default(), Some(tail), &[]).unwrap();
    assert_eq!(f.read(loc).unwrap(), keep);
    assert_eq!(f.tail(), tail, "the fold must resume exactly at the committed tail");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_only_committed_prefix_ignores_damaged_later_bytes() {
    let dir = tmp("read-prefix");
    let keep = b"content named by the retained manifest".to_vec();
    let (keep_loc, committed, later_loc) = {
        let mut fold = Fold::open(&dir, FoldCfg::default()).unwrap();
        let keep_loc = fold.put(&keep).unwrap().loc;
        let committed = fold.sync().unwrap();
        let later_loc = fold.put(b"append residue from a newer commit").unwrap().loc;
        fold.sync().unwrap();
        (keep_loc, committed, later_loc)
    };

    // Damage only the suffix beyond the retained manifest's authority. Recovery of that older
    // candidate must neither trust nor reject bytes the candidate does not name.
    {
        let path = dir.join(format!("seg-{:08}.fold", committed.seg));
        let file = OpenOptions::new().read(true).write(true).open(path).unwrap();
        let mut byte = [0u8; 1];
        read_at(&file, &mut byte, committed.off as u64 + 20).unwrap();
        byte[0] ^= 0xff;
        write_at(&file, &byte, committed.off as u64 + 20).unwrap();
        file.sync_all().unwrap();
    }

    let fold = Fold::open_read_at(&dir, FoldCfg::default(), committed).unwrap();
    assert_eq!(fold.read(keep_loc).unwrap(), keep);
    assert!(fold.read(later_loc).is_err(), "the bounded reader must not expose a newer suffix");
    assert_eq!(fold.scrub().unwrap().trailing_uncommitted, 0);

    let physical =
        std::fs::metadata(dir.join(format!("seg-{:08}.fold", committed.seg))).unwrap().len();
    let beyond = FoldTail { seg: committed.seg, off: u32::try_from(physical + 1).unwrap() };
    assert!(
        Fold::open_read_at(&dir, FoldCfg::default(), beyond).is_err(),
        "a retained manifest cannot claim bytes that do not exist"
    );
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
        locs.push(
            f.put(format!("{{\"role\":\"user\",\"content\":\"message {i}\"}}").as_bytes())
                .unwrap()
                .loc,
        );
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
        locs.push(
            f.put(format!("piece number {i} with some padding to give it size").as_bytes())
                .unwrap()
                .loc,
        );
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
        s.hits,
        s.misses
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_configuration_that_would_overflow_is_refused_at_open() {
    let d = tmp("cfgguard");
    // A block is admitted into a fresh segment however large, so block_target is what bounds the
    // segment append point and Loc.in_off — both u32. Past 4 GiB they wrap, and in release that is a
    // block directory pointing at the wrong offset with no error anywhere.
    let huge = FoldCfg { block_target: 5 << 30, ..FoldCfg::default() };
    assert!(
        Fold::open(&d.join("f1"), huge).is_err(),
        "an overflowing block_target must be refused"
    );

    let zero = FoldCfg { block_target: 0, ..FoldCfg::default() };
    assert!(Fold::open(&d.join("f2"), zero).is_err(), "a zero block_target must be refused");

    for lvl in [0i32, -3, 23, 100] {
        let bad = FoldCfg { level: lvl, ..FoldCfg::default() };
        assert!(
            Fold::open(&d.join(format!("f{lvl}")), bad).is_err(),
            "zstd level {lvl} is out of range and must be refused at open, not at first write"
        );
    }

    // and the defaults are, of course, accepted
    assert!(Fold::open(&d.join("ok"), FoldCfg::default()).is_ok());
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn recovery_rolls_back_across_segment_boundaries() {
    // Everything about recovery has been exercised inside ONE segment. The multi-segment path is
    // different code: it deletes whole segments above the committed one, pops their headers, and
    // rebuilds the block directory across the survivors.
    let d = tmp("multiseg");
    let cfg = FoldCfg { seg_max: 1 << 18, block_target: 1 << 15, ..FoldCfg::default() };

    let (committed, want) = {
        let mut f = Fold::open(&d, cfg).unwrap();
        let mut want = Vec::new();
        // enough to fill several segments
        for i in 0..400u32 {
            let piece: Vec<u8> = (0..64u32)
                .flat_map(|j| blake3::hash(&(i * 1000 + j).to_le_bytes()).as_bytes().to_vec())
                .collect();
            let p = f.put(&piece).unwrap();
            want.push((p.hash, p.loc, piece));
        }
        let tail = f.sync().unwrap();
        assert!(
            tail.seg > 0,
            "the test must actually cross a segment boundary; got seg {}",
            tail.seg
        );

        // more content AFTER the committed tail, spilling into further segments
        for i in 400..800u32 {
            let piece: Vec<u8> = (0..64u32)
                .flat_map(|j| blake3::hash(&(i * 1000 + j).to_le_bytes()).as_bytes().to_vec())
                .collect();
            f.put(&piece).unwrap();
        }
        f.sync().unwrap();
        let after = f.segment_count();
        assert!(after > tail.seg + 1, "the uncommitted writes must have rolled further");
        drop(f);
        (tail, want)
    };

    // Recover to the committed tail: segments above it must go, and everything at or below must read.
    let f = Fold::open_at(&d, cfg, Some(committed), &[]).unwrap();
    assert_eq!(
        f.segment_count(),
        committed.seg + 1,
        "segments above the committed tail must be removed"
    );
    for (hash, loc, piece) in &want {
        let got = f.read_verified(*loc, *hash).unwrap();
        assert_eq!(&got, piece, "a committed piece did not survive a multi-segment rollback");
    }
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn a_committed_tail_beyond_the_data_is_refused() {
    // The disk broke an fsync promise. Serving a fold that silently lost durable bytes is worse than
    // refusing to open.
    let d = tmp("liartail");
    let cfg = FoldCfg { seg_max: 1 << 18, block_target: 1 << 15, ..FoldCfg::default() };
    {
        let mut f = Fold::open(&d, cfg).unwrap();
        for i in 0..50u32 {
            f.put(blake3::hash(&i.to_le_bytes()).as_bytes().as_ref()).unwrap();
        }
        f.sync().unwrap();
    }
    let beyond = FoldTail { seg: 99, off: 4096 };
    assert!(
        Fold::open_at(&d, cfg, Some(beyond), &[]).is_err(),
        "a committed tail past the last good block must refuse, not truncate to it"
    );
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn sealed_segments_carry_sidecars_and_survive_losing_them() {
    // The directory sidecar is what makes open O(active segment) instead of O(store) — and it is
    // ADVISORY: absent, torn, or stale, the segment is rescanned and the answer is identical.
    let dir = tmp("sidecar");
    let cfg = FoldCfg { seg_max: 256 * 1024, block_target: 32 * 1024, ..Default::default() };
    let mut want = Vec::new();
    {
        let mut f = Fold::open(&dir, cfg).unwrap();
        for b in corpus() {
            let p = f.put(&b).unwrap();
            want.push((p.loc, p.hash, b));
        }
        f.sync().unwrap();
        assert!(f.segment_count() > 1, "the corpus must roll at least one segment");
    }
    let segs = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".fold"))
        .count();
    let sidecars = || {
        std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".dir"))
            .count()
    };
    assert_eq!(
        sidecars(),
        segs - 1,
        "every SEALED segment rolls with a sidecar; the active has none"
    );

    // Reads through sidecar-built directories are byte-exact.
    {
        let f = Fold::open(&dir, cfg).unwrap();
        for (loc, hash, b) in &want {
            assert_eq!(&f.read_verified(*loc, *hash).unwrap(), b, "sidecar-directed read drifted");
        }
    }

    // Delete one sidecar and corrupt another: open falls back to the scan, answers identically,
    // and the WRITER regenerates what was lost.
    std::fs::remove_file(dir.join("seg-00000000.dir")).unwrap();
    if segs > 2 {
        let p = dir.join("seg-00000001.dir");
        let mut b = std::fs::read(&p).unwrap();
        let mid = b.len() / 2;
        b[mid] ^= 0xFF;
        std::fs::write(&p, &b).unwrap();
    }
    {
        let f = Fold::open(&dir, cfg).unwrap();
        for (loc, hash, b) in &want {
            assert_eq!(&f.read_verified(*loc, *hash).unwrap(), b, "fallback-scan read drifted");
        }
    }
    assert_eq!(sidecars(), segs - 1, "the writer must regenerate missing or damaged sidecars");

    // A stale sidecar — right checksum, wrong length for the file — is refused and rescanned.
    let p = dir.join("seg-00000000.dir");
    let good = std::fs::read(&p).unwrap();
    {
        // shrink the SEGMENT's sidecar claim by rebuilding one for a different length: simulate by
        // truncating nothing and instead poking the tail field then re-checksumming
        let mut b = good.clone();
        let tail = u32::from_le_bytes(b[12..16].try_into().unwrap());
        b[12..16].copy_from_slice(&(tail - 1).to_le_bytes());
        let n = b.len();
        let crc = crc32fast::hash(&b[..n - 4]);
        b[n - 4..].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&p, &b).unwrap();
    }
    {
        let f = Fold::open(&dir, cfg).unwrap();
        for (loc, hash, b) in &want {
            assert_eq!(
                &f.read_verified(*loc, *hash).unwrap(),
                b,
                "stale sidecar must be refused, not trusted"
            );
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn scrub_verifies_every_frame_and_condemns_a_damaged_sealed_segment() {
    let dir = tmp("scrub");
    let cfg = FoldCfg { seg_max: 128 * 1024, block_target: 16 * 1024, ..Default::default() };
    {
        let mut f = Fold::open(&dir, cfg).unwrap();
        for b in corpus() {
            f.put(&b).unwrap();
        }
        f.sync().unwrap();
        assert!(f.segment_count() > 1);
        let report = f.scrub().unwrap();
        assert!(report.blocks > 2, "a real fold scrubs real blocks: {report:?}");
        assert_eq!(report.trailing_uncommitted, 0, "a synced fold has no residue");
    }
    // one flipped byte inside a SEALED segment's frame region must condemn it
    let seg0 = dir.join("seg-00000000.fold");
    let mut b = std::fs::read(&seg0).unwrap();
    let mid = 48 + (b.len() - 48) / 2;
    b[mid] ^= 0x01;
    std::fs::write(&seg0, &b).unwrap();
    let f = Fold::open_read(&dir, cfg).unwrap();
    assert!(f.scrub().is_err(), "a damaged sealed segment must fail the scrub");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_segment_claiming_encryption_or_an_unknown_flag_refuses() {
    // Reject-forward: the encryption bit is reserved and REFUSED, so if encryption is ever built
    // every reader shipped before it declines rather than serving ciphertext as content. The
    // refusal names encryption, because that sends an operator somewhere different from
    // "unknown flags".
    let dir = tmp("encflag");
    {
        let mut f = Fold::open(&dir, FoldCfg::default()).unwrap();
        f.put(b"content a future build might have encrypted").unwrap();
        f.sync().unwrap();
    }
    let seg = dir.join("seg-00000000.fold");
    let pristine = std::fs::read(&seg).unwrap();

    let mut b = pristine.clone();
    b[12..16].copy_from_slice(&turndb::fold::segment::SEG_FLAG_ENCRYPTED.to_le_bytes());
    std::fs::write(&seg, &b).unwrap();
    let err = match Fold::open_read(&dir, FoldCfg::default()) {
        Err(e) => format!("{e:#}"),
        Ok(_) => panic!("a segment claiming encryption must refuse"),
    };
    assert!(err.contains("ENCRYPTED"), "the refusal must name encryption: {err}");

    let mut b = pristine;
    b[12..16].copy_from_slice(&(1u32 << 17).to_le_bytes());
    std::fs::write(&seg, &b).unwrap();
    assert!(Fold::open_read(&dir, FoldCfg::default()).is_err(), "unknown flags must refuse");
    std::fs::remove_dir_all(&dir).ok();
}

/// The fold living inside a container: blocks append into a growing member, a roll seals one
/// member and begins the next, the writer reads its own appends through the staged view, and
/// recovery is arithmetic — uncommitted growth does not exist after a reopen, with no truncate
/// and no unlink anywhere.
#[test]
fn a_fold_lives_inside_a_container_and_recovers_by_reading() {
    use std::sync::{Arc, Mutex};
    use turndb::container::Container;
    use turndb::read_limits::ReadLimits;

    let root = tmp("in-container");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("fold.turndb");
    drop(Container::create(&ct).unwrap());

    let cfg = FoldCfg { block_target: 4096, seg_max: 16 * 1024, ..Default::default() };
    let c = Arc::new(Mutex::new(Container::open(&ct).unwrap()));
    let mut f =
        Fold::open_container_writer(c.clone(), 0, cfg, None, &[], ReadLimits::default()).unwrap();

    // Enough incompressible pieces to seal several blocks and roll at least one segment.
    let corpus = corpus();
    let mut placed = Vec::new();
    for d in corpus.iter().cycle().take(96) {
        let p = f.put(d).unwrap();
        placed.push((p.loc, p.hash, d.clone()));
    }
    let tail = f.sync().unwrap();
    assert!(f.segment_count() > 1, "the fixture must roll across members");

    // The writer reads its own appends before any commit: the staged view serves them.
    for (loc, hash, d) in &placed {
        assert_eq!(&f.read_verified(*loc, *hash).unwrap(), d, "read-your-writes inside the file");
    }

    // Publish, exactly as a native flush would after building its part.
    c.lock().unwrap().commit().unwrap();
    drop(f);

    // Post-commit appends that are NEVER committed: staged extents that must vanish by reading.
    {
        let mut f2 =
            Fold::open_container_writer(c.clone(), 0, cfg, Some(tail), &[], ReadLimits::default())
                .unwrap();
        for d in corpus.iter().take(8) {
            f2.put(d).unwrap();
        }
        f2.sync().unwrap();
        drop(f2);
    }
    drop(c); // the staged, uncommitted state dies with the handle — this is the crash

    let c2 = Arc::new(Mutex::new(Container::open(&ct).unwrap()));
    let f3 =
        Fold::open_container_writer(c2.clone(), 0, cfg, Some(tail), &[], ReadLimits::default())
            .unwrap();
    assert_eq!(f3.tail(), tail, "uncommitted growth does not exist after a reopen");
    for (loc, hash, d) in &placed {
        assert_eq!(&f3.read_verified(*loc, *hash).unwrap(), d, "committed pieces survive");
    }

    // The sealed segment's advisory sidecar rode the same commit as the blocks it describes.
    {
        let c = c2.lock().unwrap();
        assert!(c.contains("fold/seg-00000000.fold"));
        assert!(c.contains("fold/seg-00000001.fold"));
        assert!(c.contains("fold/seg-00000000.dir"), "the sealed member's sidecar is a member");
        let active_member = format!("fold/seg-{:08}.fold", tail.seg);
        assert_eq!(
            c.member_len(&active_member).unwrap(),
            u64::from(tail.off),
            "the committed member length IS the tail"
        );
    }

    // A manifest that disagrees with the container must refuse, not roll back.
    let wrong = FoldTail { seg: tail.seg, off: tail.off + 64 };
    let err = Fold::open_container_writer(c2, 0, cfg, Some(wrong), &[], ReadLimits::default())
        .map(|_| ())
        .unwrap_err();
    assert!(err.to_string().contains("disagree"), "got: {err:#}");
    std::fs::remove_dir_all(&root).ok();
}

/// Positioned read/write for the byte flips below, on every OS the suite runs on.
fn read_at(f: &std::fs::File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(f, buf, off)
    }
    #[cfg(windows)]
    {
        let n = std::os::windows::fs::FileExt::seek_read(f, buf, off)?;
        assert_eq!(n, buf.len(), "short positioned read");
        Ok(())
    }
}

fn write_at(f: &std::fs::File, buf: &[u8], off: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_all_at(f, buf, off)
    }
    #[cfg(windows)]
    {
        let n = std::os::windows::fs::FileExt::seek_write(f, buf, off)?;
        assert_eq!(n, buf.len(), "short positioned write");
        Ok(())
    }
}
