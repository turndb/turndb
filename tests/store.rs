//! Step-3 gate: durability and recovery. A crash at any point loses nothing that was ACKed, and a
//! reader works from the files alone — no lock, no writer, no daemon.

use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::store::{Manifest, PartRef, Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-store-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    // small blocks so tests exercise sealing rather than sitting in one open buffer
    FoldCfg { block_target: 8 * 1024, ..Default::default() }
}

const M1: &[u8] = b"{\"role\":\"user\",\"content\":\"the first message, long enough to be worth folding\"}";
const M2: &[u8] = b"{\"role\":\"assistant\",\"content\":\"the second message, also reasonably long\"}";

fn put(s: &mut Store, id: &str, extra: &[u8]) -> Vec<u8> {
    let spans = vec![Span::Lit(b"["), Span::Piece(M1), Span::Lit(b","), Span::Piece(M2), Span::Lit(b","), Span::Piece(extra), Span::Lit(b"]")];
    let attrs = vec![
        ("model".into(), AttrValue::Str("claude".into())),
        ("n".into(), AttrValue::Int(id.len() as i64)),
        ("ok".into(), AttrValue::Bool(true)),
    ];
    s.put(id, &spans, attrs).unwrap();
    let mut want = Vec::new();
    want.extend_from_slice(b"[");
    want.extend_from_slice(M1);
    want.extend_from_slice(b",");
    want.extend_from_slice(M2);
    want.extend_from_slice(b",");
    want.extend_from_slice(extra);
    want.extend_from_slice(b"]");
    want
}

#[test]
fn put_flush_get_is_byte_exact() {
    let dir = tmp("basic");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut want = Vec::new();
    for i in 0..200 {
        want.push((format!("rec:{i:04}"), put(&mut s, &format!("rec:{i:04}"), format!("unique body {i}").as_bytes())));
    }
    // readable before the flush, straight from the memtable
    assert_eq!(s.reconstruct(&want[7].0).unwrap().unwrap(), want[7].1);
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(s.part_count(), 1);
    assert_eq!(s.memtable_len(), 0);
    assert_eq!(s.wal_bytes(), 0, "the log is redundant once its records are in a committed part");

    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "BYTE DRIFT for {id}");
        let r = s.get(id).unwrap().unwrap();
        assert_eq!(r.attrs[0].1, AttrValue::Str("claude".into()));
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn crash_before_flush_recovers_from_the_log() {
    let dir = tmp("crashwal");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for i in 0..50 {
            want.push((format!("r{i:03}"), put(&mut s, &format!("r{i:03}"), format!("body {i}").as_bytes())));
        }
        s.sync().unwrap(); // ACKed — must survive
        // simulate the process dying: no flush, no manifest commit, unsynced blocks lost.
        // (drop releases the writer lock, which a crash also does.)
        drop(s);
    }
    let s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(s.memtable_len(), 50, "ACKed records must come back in the memtable");
    assert_eq!(s.part_count(), 0);
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "lost or corrupted {id} across a crash");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn crash_after_flush_keeps_the_part_and_empties_the_log() {
    let dir = tmp("crashpart");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for i in 0..40 {
            want.push((format!("p{i:03}"), put(&mut s, &format!("p{i:03}"), format!("b{i}").as_bytes())));
        }
        s.sync().unwrap();
        s.flush().unwrap();
        drop(s);
    }
    let s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(s.part_count(), 1);
    assert_eq!(s.memtable_len(), 0, "committed records must not replay into the memtable");
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_part_the_manifest_never_named_is_ignored() {
    // The manifest is the only commit point: a part file on disk that the manifest does not name was
    // written by a flush that crashed before committing, and must be invisible.
    let dir = tmp("orphan");
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        put(&mut s, "kept", b"x");
        s.sync().unwrap();
        s.flush().unwrap();
        drop(s);
    }
    let real = std::fs::read_dir(&dir).unwrap().flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".part"))
        .expect("a committed part exists");
    std::fs::copy(&real, dir.join("part-99999999.part")).unwrap();
    let s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(s.part_count(), 1, "an uncommitted part file must be ignored");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn torn_log_tail_keeps_the_intact_prefix() {
    use std::io::Write;
    let dir = tmp("tornwal");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for i in 0..20 {
            want.push((format!("t{i:03}"), put(&mut s, &format!("t{i:03}"), format!("b{i}").as_bytes())));
        }
        s.sync().unwrap();
        drop(s);
    }
    // a crash mid-append leaves a frame header promising bytes that never landed
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.join("WAL")).unwrap();
        f.write_all(&[0x57, 99, 0, 0, 0, 0, 0, 0, 0, 200, 0, 0, 0]).unwrap();
        f.write_all(b"truncated").unwrap();
        f.sync_all().unwrap();
    }
    let s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(s.memtable_len(), 20, "the intact prefix must survive a torn tail");
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn newest_wins_across_parts() {
    let dir = tmp("newest");
    let mut s = Store::open(&dir, cfg()).unwrap();
    put(&mut s, "dup", b"first version");
    s.sync().unwrap();
    s.flush().unwrap();
    let second = put(&mut s, "dup", b"SECOND version");
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(s.part_count(), 2);
    assert_eq!(s.reconstruct("dup").unwrap().unwrap(), second, "the later part must win");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_reader_needs_no_lock_no_writer_no_daemon() {
    let dir = tmp("readonly");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut want = Vec::new();
    for i in 0..30 {
        want.push((format!("q{i:03}"), put(&mut s, &format!("q{i:03}"), format!("body {i}").as_bytes())));
    }
    s.sync().unwrap();
    s.flush().unwrap();

    // uncommitted work that a reader must NOT see
    put(&mut s, "uncommitted", b"not yet");
    s.sync().unwrap();

    // the writer is still live and holding its lock
    let r = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r.part_count(), 1);
    for (id, body) in &want {
        assert_eq!(&r.reconstruct(id).unwrap().unwrap(), body, "reader lost {id}");
    }
    assert!(r.get("uncommitted").unwrap().is_none(), "a reader must see only the committed manifest");
    assert_eq!(r.ids().unwrap().len(), 30);

    // and a second concurrent reader is fine
    let r2 = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r2.reconstruct(&want[3].0).unwrap().unwrap(), want[3].1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_second_writer_is_refused() {
    let dir = tmp("twowriters");
    let _s = Store::open(&dir, cfg()).unwrap();
    assert!(Store::open(&dir, cfg()).is_err(), "single-writer must be enforced, not assumed");
    // but reading is always allowed
    assert!(Store::open_read(&dir, cfg()).is_ok());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dedup_survives_flush_and_reopen() {
    let dir = tmp("dedup");
    let shared = "a system prompt shared by every record. ".repeat(40).into_bytes();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for i in 0..30 {
            s.put(&format!("d{i:03}"), &[Span::Piece(&shared)], Vec::new()).unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
        drop(s);
    }
    let before = std::fs::read_dir(dir.join("fold")).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len()).sum::<u64>();

    let mut s = Store::open(&dir, cfg()).unwrap();
    for i in 30..60 {
        s.put(&format!("d{i:03}"), &[Span::Piece(&shared)], Vec::new()).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    let after = std::fs::read_dir(dir.join("fold")).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len()).sum::<u64>();

    // NOTE: cross-flush dedup currently relies on the in-memory window, which survives within one
    // process but not across a reopen. Tier-1 (per-part Bloom + hash column) is what makes this
    // hold across processes; until then a reopened writer may re-append known content.
    assert!(after >= before);
    for i in 0..60 {
        assert_eq!(s.reconstruct(&format!("d{i:03}")).unwrap().unwrap(), shared);
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Step-4 gate: merge consolidates parts without touching the fold, and never loses a record.
// ---------------------------------------------------------------------------------------------

#[test]
fn merge_consolidates_and_preserves_every_record() {
    let dir = tmp("merge");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut want = Vec::new();
    for batch in 0..5 {
        for i in 0..20 {
            let id = format!("m{batch}-{i:03}");
            want.push((id.clone(), put(&mut s, &id, format!("body {batch}/{i}").as_bytes())));
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    assert_eq!(s.part_count(), 5);
    let fold_before: u64 = std::fs::read_dir(dir.join("fold")).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len()).sum();

    let st = s.merge_range(0, 5).unwrap().unwrap();
    assert_eq!(st.inputs, 5);
    assert_eq!(st.records_out, 100);
    assert_eq!(st.fold_bytes_touched, 0);
    assert_eq!(s.part_count(), 1, "five parts must become one");

    let fold_after: u64 = std::fs::read_dir(dir.join("fold")).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len()).sum();
    assert_eq!(fold_after, fold_before, "MERGE MUST NOT REWRITE CONTENT");

    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "merge lost or corrupted {id}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merge_keeps_the_newest_version_of_a_reput_id() {
    let dir = tmp("mergedup");
    let mut s = Store::open(&dir, cfg()).unwrap();
    put(&mut s, "same", b"version one");
    s.sync().unwrap(); s.flush().unwrap();
    let other = put(&mut s, "other", b"unrelated");
    s.sync().unwrap(); s.flush().unwrap();
    let newest = put(&mut s, "same", b"version THREE");
    s.sync().unwrap(); s.flush().unwrap();
    assert_eq!(s.part_count(), 3);

    let st = s.merge_range(0, 3).unwrap().unwrap();
    assert_eq!(st.records_out, 2, "two distinct ids survive");
    assert_eq!(st.superseded, 1);
    assert_eq!(s.reconstruct("same").unwrap().unwrap(), newest, "the newest version must survive a merge");
    assert_eq!(s.reconstruct("other").unwrap().unwrap(), other);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_merged_store_survives_reopen_and_sweeps_its_inputs() {
    let dir = tmp("mergereopen");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for b in 0..4 {
            for i in 0..10 {
                let id = format!("s{b}-{i:02}");
                want.push((id.clone(), put(&mut s, &id, format!("x{b}{i}").as_bytes())));
            }
            s.sync().unwrap(); s.flush().unwrap();
        }
        s.merge_range(0, 4).unwrap().unwrap();
        drop(s);
    }
    let files: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".part")).collect();
    assert_eq!(files.len(), 1, "superseded inputs must be swept, leaving only the merged part: {files:?}");

    let s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(s.part_count(), 1);
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body);
    }
    // and a plain reader sees the merged state with no lock
    let r = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r.ids().unwrap().len(), 40);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tiering_bounds_part_count_under_sustained_writes() {
    let dir = tmp("tiering");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut want = Vec::new();
    let mut peak = 0usize;
    for b in 0..30 {
        for i in 0..5 {
            let id = format!("t{b:02}-{i}");
            want.push((id.clone(), put(&mut s, &id, format!("v{b}{i}").as_bytes())));
        }
        s.sync().unwrap();
        s.flush().unwrap();
        s.maybe_compact(8, 4).unwrap();
        peak = peak.max(s.part_count());
    }
    assert!(peak <= 8, "tiering must bound part count; peaked at {peak}");
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "tiering lost {id}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Tier-1 dedup: content already committed to a part is never stored twice, no matter how much
// time, how many flushes, or a process restart separates the two writes.
// ---------------------------------------------------------------------------------------------

/// Total bytes of fold segments — the only thing that grows when content is genuinely stored.
fn fold_bytes(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir.join("fold")).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len()).sum()
}

#[test]
fn content_repeated_after_a_flush_costs_nothing() {
    let dir = tmp("tier1");
    let mut s = Store::open(&dir, cfg()).unwrap();
    // A payload big enough that storing it twice is unmistakable in the segment size.
    let payload: Vec<u8> = (0..200_000u32).flat_map(|i| blake3::hash(&i.to_le_bytes()).as_bytes()[..4].to_vec()).collect();

    s.put("first", &[Span::Piece(&payload)], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    let after_first = fold_bytes(&dir);

    // Same content, new record, new flush window. Tier 0 was released at the flush, so only a part
    // lookup can catch this.
    s.put("second", &[Span::Piece(&payload)], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    let after_second = fold_bytes(&dir);

    assert_eq!(after_second, after_first,
        "TIER-1 MISS: {} bytes of already-stored content were written again",
        after_second - after_first);
    assert_eq!(s.reconstruct("first").unwrap().unwrap(), payload);
    assert_eq!(s.reconstruct("second").unwrap().unwrap(), payload, "the dedup'd record must still read back exactly");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dedup_survives_a_process_restart() {
    let dir = tmp("tier1reopen");
    let payload: Vec<u8> = (0..100_000u32).flat_map(|i| blake3::hash(&i.to_le_bytes()).as_bytes()[..4].to_vec()).collect();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        s.put("a", &[Span::Piece(&payload)], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
        drop(s);
    }
    let before = fold_bytes(&dir);
    {
        // A fresh process has no in-memory window whatsoever. Only the parts on disk can answer.
        let mut s = Store::open(&dir, cfg()).unwrap();
        s.put("b", &[Span::Piece(&payload)], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
        assert_eq!(s.reconstruct("a").unwrap().unwrap(), payload);
        assert_eq!(s.reconstruct("b").unwrap().unwrap(), payload);
        drop(s);
    }
    assert_eq!(fold_bytes(&dir), before, "dedup must not depend on process lifetime");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dedup_survives_a_merge() {
    let dir = tmp("tier1merge");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let payload: Vec<u8> = (0..80_000u32).flat_map(|i| blake3::hash(&i.to_le_bytes()).as_bytes()[..4].to_vec()).collect();
    for i in 0..4 {
        s.put(&format!("r{i}"), &[Span::Piece(&payload)], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let before = fold_bytes(&dir);
    s.merge_range(0, 4).unwrap().unwrap();
    // The merged part must carry the dictionary forward, filter and permutation included.
    s.put("after-merge", &[Span::Piece(&payload)], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(fold_bytes(&dir), before, "a merge must not blind the dedup index");
    for i in 0..4 {
        assert_eq!(s.reconstruct(&format!("r{i}")).unwrap().unwrap(), payload);
    }
    assert_eq!(s.reconstruct("after-merge").unwrap().unwrap(), payload);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_heavily_repeated_corpus_stores_one_copy_of_each_distinct_piece() {
    let dir = tmp("tier1corpus");
    let mut s = Store::open(&dir, cfg()).unwrap();
    // 20 distinct pieces, referenced 500 times across 100 flushes.
    let pieces: Vec<Vec<u8>> = (0..20u32)
        .map(|i| (0..2000u32).flat_map(|j| blake3::hash(&(i * 100_000 + j).to_le_bytes()).as_bytes()[..8].to_vec()).collect())
        .collect();
    let distinct: u64 = pieces.iter().map(|p| p.len() as u64).sum();
    for round in 0..100 {
        for (i, p) in pieces.iter().enumerate() {
            s.put(&format!("r{round}-{i}"), &[Span::Piece(p)], vec![]).unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let stored = fold_bytes(&dir);
    // The content is deliberately incompressible, so one stored copy plus framing is a touch OVER the
    // raw distinct size. The claim is the ratio: 100 rounds cost what one round costs.
    let naive = distinct * 100;
    assert!(stored < distinct * 11 / 10,
        "stored {stored} vs {distinct} distinct bytes — dedup did not hold across 100 flushes");
    eprintln!("100 rounds x 20 pieces: {stored} B stored, {naive} B without dedup ({:.0}x)", naive as f64 / stored as f64);
    for (i, p) in pieces.iter().enumerate() {
        assert_eq!(&s.reconstruct(&format!("r99-{i}")).unwrap().unwrap(), p);
        assert_eq!(&s.reconstruct(&format!("r0-{i}")).unwrap().unwrap(), p);
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Regressions found by audit. Each of these was reachable in ordinary operation.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_crash_after_tier1_dedup_does_not_wedge_the_store() {
    // Tier-1 made this reachable: a piece deduped against an OLDER PART carries no WAL bytes, so
    // replay never puts it in the fold's window. A flush that resolved only through the window then
    // failed forever — records unreadable, WAL unbounded. On a high-duplication corpus that is nearly
    // every record after the first flush.
    let dir = tmp("wedge");
    let payload: Vec<u8> = (0..40_000u32)
        .flat_map(|i| blake3::hash(&i.to_le_bytes()).as_bytes()[..8].to_vec()).collect();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        s.put("first", &[Span::Piece(&payload)], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap(); // now committed to a part
        drop(s);
    }
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        // Deduped against the part on disk -> no bytes in the WAL, by design.
        s.put("second", &[Span::Piece(&payload)], vec![]).unwrap();
        s.sync().unwrap();
        drop(s); // CRASH: synced but never flushed
    }
    let mut s = Store::open(&dir, cfg()).unwrap();
    let flushed = s.flush().expect("a crash after a Tier-1 dedup must not wedge the flush path");
    assert!(flushed.is_some(), "the staged record must reach a part");
    assert_eq!(s.reconstruct("first").unwrap().unwrap(), payload);
    assert_eq!(s.reconstruct("second").unwrap().unwrap(), payload,
        "the record staged before the crash must survive it");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unreadable_manifest_is_an_error_not_an_empty_store() {
    // The orphan sweep made this destructive: an unreadable manifest yielded the DEFAULT manifest,
    // and the sweep then unlinked every part it did not name.
    let dir = tmp("badmanifest");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for i in 0..20 {
            let id = format!("k{i:02}");
            want.push((id.clone(), put(&mut s, &id, format!("v{i}").as_bytes())));
        }
        s.sync().unwrap();
        s.flush().unwrap();
        drop(s);
    }
    let parts_before = std::fs::read_dir(&dir).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part")).count();
    assert_eq!(parts_before, 1);

    // A read that fails for a reason OTHER than absence. A directory reads as EISDIR everywhere.
    let man = dir.join("MANIFEST");
    std::fs::remove_file(&man).unwrap();
    std::fs::create_dir(&man).unwrap();

    assert!(Store::open(&dir, cfg()).is_err(), "an unreadable manifest must refuse to open");
    let parts_after = std::fs::read_dir(&dir).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part")).count();
    assert_eq!(parts_after, 1, "REFUSING TO OPEN MUST NOT DELETE DATA");

    // and once the manifest is readable again the store is intact
    std::fs::remove_dir(&man).unwrap();
    std::fs::write(&man, serde_json::to_vec(&Manifest {
        parts: vec![PartRef { file: "part-00000001.part".into(), seq_lo: 1, seq_hi: 1, records: 20 }],
        fold_seg: 0, fold_off: 0, next_seq: 1,
    }).unwrap()).unwrap();
    let s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(s.part_count(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merging_an_interior_run_does_not_unlink_its_own_output() {
    // merge_range is public and its `lo` exists precisely for this. The output used to be named from
    // (seq_hi, len), which is not unique — a collision meant the post-commit sweep deleted the part
    // the manifest had just committed, and the store never opened again.
    let dir = tmp("interior");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut want = Vec::new();
    for b in 0..6 {
        for i in 0..6 {
            let id = format!("p{b}-{i}");
            want.push((id.clone(), put(&mut s, &id, format!("body {b}/{i}").as_bytes())));
        }
        s.sync().unwrap(); s.flush().unwrap();
    }
    // Two interior merges arranged so the second's output shares (seq_hi, len) with one of its own
    // INPUTS — the case the old (seq_hi, len) naming could not distinguish.
    //   parts: [1][2][3][4][5][6]
    s.merge_range(3, 2).unwrap().unwrap(); //  -> [1][2][3][4-5][6]   seq_hi=5, len=2
    s.merge_range(2, 2).unwrap().unwrap(); //  -> [1][2][3-5][6]      seq_hi=5, len=2  <- same name
    assert_eq!(s.part_count(), 4);
    drop(s);

    let s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(s.part_count(), 4, "the merged part must survive reopen");
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "interior merge lost {id}");
    }
    std::fs::remove_dir_all(&dir).ok();
}
