//! Step-3 gate: durability and recovery. A crash at any point loses nothing that was ACKed, and a
//! reader works from the files alone — no lock, no writer, no daemon.

use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::store::{ContentSpans, Manifest, PartRef, Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-store-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    // small blocks so tests exercise sealing rather than sitting in one open buffer
    FoldCfg { block_target: 8 * 1024, ..Default::default() }
}

const M1: &[u8] =
    b"{\"role\":\"user\",\"content\":\"the first message, long enough to be worth folding\"}";
const M2: &[u8] =
    b"{\"role\":\"assistant\",\"content\":\"the second message, also reasonably long\"}";

fn put(s: &mut Store, id: &str, extra: &[u8]) -> Vec<u8> {
    let spans = vec![
        Span::Lit(b"["),
        Span::Piece(M1),
        Span::Lit(b","),
        Span::Piece(M2),
        Span::Lit(b","),
        Span::Piece(extra),
        Span::Lit(b"]"),
    ];
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
        want.push((
            format!("rec:{i:04}"),
            put(&mut s, &format!("rec:{i:04}"), format!("unique body {i}").as_bytes()),
        ));
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
fn named_content_is_independent_sparse_and_content_addressed() {
    let dir = tmp("named-content");
    let shared = b"the same large content appears under request and response";
    let mut s = Store::open(&dir, cfg()).unwrap();

    // Deliberately submit names out of order. The semantic record is a map and reads canonically.
    let contents = vec![
        ContentSpans::new("response", vec![Span::Piece(shared), Span::Lit(b"!")]),
        ContentSpans::new("empty", vec![]),
        ContentSpans::new("request", vec![Span::Piece(shared)]),
    ];
    s.put_record("mixed:1", &contents, vec![("kind".into(), AttrValue::Str("example".into()))])
        .unwrap();

    assert_eq!(s.dedup_window_len(), 1, "one shared piece is stored once across content names");
    assert_eq!(s.reconstruct_content("mixed:1", "request").unwrap().unwrap(), shared);
    assert_eq!(
        s.reconstruct_content("mixed:1", "response").unwrap().unwrap(),
        [shared.as_slice(), b"!"].concat()
    );
    assert_eq!(s.reconstruct_content("mixed:1", "empty").unwrap(), Some(Vec::new()));
    assert_eq!(s.reconstruct_content("mixed:1", "absent").unwrap(), None);
    assert_eq!(s.reconstruct("mixed:1").unwrap(), None, "body is conventional, not privileged");
    assert_eq!(
        s.get("mixed:1")
            .unwrap()
            .unwrap()
            .contents
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["empty", "request", "response"]
    );

    s.sync().unwrap();
    s.flush().unwrap();
    let second = [ContentSpans::new("raw", vec![Span::Piece(shared)])];
    s.put_record("mixed:2", &second, vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    s.merge_range(0, 2).unwrap().unwrap();
    assert_eq!(s.part_count(), 1);
    assert_eq!(s.reconstruct_content("mixed:1", "request").unwrap().unwrap(), shared);
    assert_eq!(s.reconstruct_content("mixed:2", "raw").unwrap().unwrap(), shared);

    s.refold().unwrap();
    drop(s);
    let r = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r.reconstruct_content("mixed:1", "empty").unwrap(), Some(Vec::new()));
    assert_eq!(r.reconstruct_content("mixed:1", "absent").unwrap(), None);
    assert_eq!(r.reconstruct_content("mixed:2", "raw").unwrap().unwrap(), shared);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_invalid_content_map_has_no_storage_side_effects() {
    let dir = tmp("invalid-content-map");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let duplicate = [
        ContentSpans::new("same", vec![Span::Piece(b"first")]),
        ContentSpans::new("same", vec![Span::Piece(b"second")]),
    ];
    assert!(s.put_record("bad", &duplicate, vec![]).is_err());
    assert_eq!(s.memtable_len(), 0);
    assert_eq!(s.dedup_window_len(), 0);
    assert_eq!(s.wal_bytes(), 0);

    let empty_name = [ContentSpans::new("", vec![Span::Piece(b"bytes")])];
    assert!(s.put_record("bad", &empty_name, vec![]).is_err());
    assert_eq!(s.memtable_len(), 0);
    assert_eq!(s.dedup_window_len(), 0);
    assert_eq!(s.wal_bytes(), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_invalid_late_batch_member_has_no_storage_side_effects() {
    let dir = tmp("invalid-batch-record");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut batch = turndb::store::Batch::new();
    batch.put("good", &[Span::Piece(b"must not reach the fold")], vec![]);
    // The compatibility staging API cannot return an error, so apply preflights the entire batch.
    batch.put("", &[Span::Piece(b"also must not reach the fold")], vec![]);
    assert!(s.apply(batch).is_err());
    assert_eq!(s.memtable_len(), 0);
    assert_eq!(s.dedup_window_len(), 0);
    assert_eq!(s.wal_bytes(), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn crash_before_flush_recovers_from_the_log() {
    let dir = tmp("crashwal");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for i in 0..50 {
            want.push((
                format!("r{i:03}"),
                put(&mut s, &format!("r{i:03}"), format!("body {i}").as_bytes()),
            ));
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
        assert_eq!(
            &s.reconstruct(id).unwrap().unwrap(),
            body,
            "lost or corrupted {id} across a crash"
        );
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
            want.push((
                format!("p{i:03}"),
                put(&mut s, &format!("p{i:03}"), format!("b{i}").as_bytes()),
            ));
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
    let real = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
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
            want.push((
                format!("t{i:03}"),
                put(&mut s, &format!("t{i:03}"), format!("b{i}").as_bytes()),
            ));
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
        want.push((
            format!("q{i:03}"),
            put(&mut s, &format!("q{i:03}"), format!("body {i}").as_bytes()),
        ));
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
    assert!(
        r.get("uncommitted").unwrap().is_none(),
        "a reader must see only the committed manifest"
    );
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
    let before = std::fs::read_dir(dir.join("fold"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len())
        .sum::<u64>();

    let mut s = Store::open(&dir, cfg()).unwrap();
    for i in 30..60 {
        s.put(&format!("d{i:03}"), &[Span::Piece(&shared)], Vec::new()).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    let after = std::fs::read_dir(dir.join("fold"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len())
        .sum::<u64>();

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
    let fold_before: u64 = std::fs::read_dir(dir.join("fold"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len())
        .sum();

    let st = s.merge_range(0, 5).unwrap().unwrap();
    assert_eq!(st.inputs, 5);
    assert_eq!(st.records_out, 100);
    assert_eq!(st.fold_bytes_touched, 0);
    assert_eq!(s.part_count(), 1, "five parts must become one");

    let fold_after: u64 = std::fs::read_dir(dir.join("fold"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len())
        .sum();
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
    s.sync().unwrap();
    s.flush().unwrap();
    let other = put(&mut s, "other", b"unrelated");
    s.sync().unwrap();
    s.flush().unwrap();
    let newest = put(&mut s, "same", b"version THREE");
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(s.part_count(), 3);

    let st = s.merge_range(0, 3).unwrap().unwrap();
    assert_eq!(st.records_out, 2, "two distinct ids survive");
    assert_eq!(st.superseded, 1);
    assert_eq!(
        s.reconstruct("same").unwrap().unwrap(),
        newest,
        "the newest version must survive a merge"
    );
    assert_eq!(s.reconstruct("other").unwrap().unwrap(), other);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_merged_store_survives_reopen_and_sweeps_its_inputs() {
    // "Sweeps" now means AFTER the retention window: replaced inputs stay on disk while a retained
    // manifest still names them — that is what keeps a reader's snapshot whole — and fall to the
    // sweep when the window prunes past their last naming manifest.
    let dir = tmp("mergereopen");
    let mut want = Vec::new();
    let part_files = |dir: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".part"))
            .collect()
    };
    let inputs;
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for b in 0..4 {
            for i in 0..10 {
                let id = format!("s{b}-{i:02}");
                want.push((id.clone(), put(&mut s, &id, format!("x{b}{i}").as_bytes())));
            }
            s.sync().unwrap();
            s.flush().unwrap();
        }
        inputs = part_files(&dir);
        assert_eq!(inputs.len(), 4);
        s.merge_range(0, 4).unwrap().unwrap();
        drop(s);
    }
    // Inside the window: every input is still pinned by a retained manifest.
    let now = part_files(&dir);
    for f in &inputs {
        assert!(
            now.contains(f),
            "input {f} is named by a retained manifest and must survive the merge"
        );
    }

    let mut s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(s.part_count(), 1, "the LIVE view is the merged part alone");
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body);
    }
    // and a plain reader sees the merged state with no lock
    let r = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r.ids().unwrap().len(), 40);
    drop(r);

    // Advance the window past every manifest that named the inputs; the sweep takes them.
    for i in 0..turndb::store::MANIFEST_RETAIN {
        put(&mut s, &format!("later-{i}"), b"z");
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let now = part_files(&dir);
    for f in &inputs {
        assert!(!now.contains(f), "input {f} outlived the retention window: {now:?}");
    }
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
    std::fs::read_dir(dir.join("fold"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len())
        .sum()
}

#[test]
fn content_repeated_after_a_flush_costs_nothing() {
    let dir = tmp("tier1");
    let mut s = Store::open(&dir, cfg()).unwrap();
    // A payload big enough that storing it twice is unmistakable in the segment size.
    let payload: Vec<u8> = (0..200_000u32)
        .flat_map(|i| blake3::hash(&i.to_le_bytes()).as_bytes()[..4].to_vec())
        .collect();

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

    assert_eq!(
        after_second,
        after_first,
        "TIER-1 MISS: {} bytes of already-stored content were written again",
        after_second - after_first
    );
    assert_eq!(s.reconstruct("first").unwrap().unwrap(), payload);
    assert_eq!(
        s.reconstruct("second").unwrap().unwrap(),
        payload,
        "the dedup'd record must still read back exactly"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dedup_survives_a_process_restart() {
    let dir = tmp("tier1reopen");
    let payload: Vec<u8> = (0..100_000u32)
        .flat_map(|i| blake3::hash(&i.to_le_bytes()).as_bytes()[..4].to_vec())
        .collect();
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
    let payload: Vec<u8> = (0..80_000u32)
        .flat_map(|i| blake3::hash(&i.to_le_bytes()).as_bytes()[..4].to_vec())
        .collect();
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
        .map(|i| {
            (0..2000u32)
                .flat_map(|j| {
                    blake3::hash(&(i * 100_000 + j).to_le_bytes()).as_bytes()[..8].to_vec()
                })
                .collect()
        })
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
    assert!(
        stored < distinct * 11 / 10,
        "stored {stored} vs {distinct} distinct bytes — dedup did not hold across 100 flushes"
    );
    eprintln!(
        "100 rounds x 20 pieces: {stored} B stored, {naive} B without dedup ({:.0}x)",
        naive as f64 / stored as f64
    );
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
        .flat_map(|i| blake3::hash(&i.to_le_bytes()).as_bytes()[..8].to_vec())
        .collect();
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
    assert_eq!(
        s.reconstruct("second").unwrap().unwrap(),
        payload,
        "the record staged before the crash must survive it"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_batch_is_all_or_nothing_across_a_crash() {
    let dir = tmp("batch");
    let mut s = Store::open(&dir, cfg()).unwrap();
    put(&mut s, "x", b"pre-batch");
    s.sync().unwrap();

    // One batch: two puts and a delete, applied, ACKed, then "crash" (drop without flush).
    let mut bt = turndb::store::Batch::new();
    bt.put("a", &[Span::Piece(b"batch content A, long enough to be worth folding")], vec![]);
    bt.put("b", &[Span::Lit(b"lit-"), Span::Piece(b"batch content B")], vec![]);
    bt.delete("x");
    s.apply(bt).unwrap();
    s.sync().unwrap();
    drop(s);

    let mut s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(
        s.reconstruct("a").unwrap().unwrap(),
        b"batch content A, long enough to be worth folding"
    );
    assert_eq!(s.reconstruct("b").unwrap().unwrap(), b"lit-batch content B");
    assert!(s.reconstruct("x").unwrap().is_none(), "the batched delete applied with the batch");

    // Another batch, ACKed — then its commit marker is torn off, as a crash mid-append would.
    // NONE of it may replay: half an export surviving is the anomaly batches exist to prevent.
    let mut bt = turndb::store::Batch::new();
    bt.put("c", &[Span::Piece(b"doomed content C")], vec![]);
    bt.put("d", &[Span::Piece(b"doomed content D")], vec![]);
    s.apply(bt).unwrap();
    s.sync().unwrap();
    drop(s);
    let wal = dir.join("WAL");
    let len = std::fs::metadata(&wal).unwrap().len();
    // the marker is the last frame: 13-byte header + 1-byte count + 4-byte crc
    std::fs::OpenOptions::new().write(true).open(&wal).unwrap().set_len(len - 18).unwrap();

    let s = Store::open(&dir, cfg()).unwrap();
    assert!(s.reconstruct("c").unwrap().is_none(), "an unsealed batch member must not replay");
    assert!(s.reconstruct("d").unwrap().is_none(), "an unsealed batch member must not replay");
    assert_eq!(
        s.reconstruct("b").unwrap().unwrap(),
        b"lit-batch content B",
        "earlier state intact"
    );
    assert!(s.reconstruct("x").unwrap().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_retained_snapshot_reads_the_past() {
    let dir = tmp("timetravel");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let v1 = put(&mut s, "k", b"v1");
    s.sync().unwrap();
    s.flush().unwrap();
    let c1 = s.manifest().commit;
    let v2 = put(&mut s, "k", b"v2");
    put(&mut s, "gone", b"soon");
    s.sync().unwrap();
    s.flush().unwrap();
    s.delete("gone").unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let r = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r.reconstruct("k").unwrap().unwrap(), v2);
    assert!(r.reconstruct("gone").unwrap().is_none());
    drop(r);

    // The snapshot at c1 is the first flush, exactly: the old version, and no `gone` — it did not
    // exist yet, rather than "was deleted".
    assert!(turndb::store::retained_commits(&dir).contains(&c1));
    let old = Store::open_read_at(&dir, cfg(), c1).unwrap();
    assert_eq!(old.reconstruct("k").unwrap().unwrap(), v1);
    assert!(old.reconstruct("gone").unwrap().is_none());
    assert_eq!(old.ids().unwrap(), vec!["k".to_string()]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_corrupt_manifest_recovers_from_the_commit_log() {
    let dir = tmp("manrecover");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for i in 0..10 {
            let id = format!("k{i}");
            want.push((id.clone(), put(&mut s, &id, format!("v{i}").as_bytes())));
        }
        s.sync().unwrap();
        s.flush().unwrap();
        drop(s);
    }
    let man = dir.join("MANIFEST");
    let mut b = std::fs::read(&man).unwrap();
    b[10] ^= 0xFF;
    std::fs::write(&man, &b).unwrap();

    assert!(Store::open(&dir, cfg()).is_err(), "a corrupt manifest must refuse, not open empty");
    let c = turndb::store::recover_manifest(&dir).unwrap();
    assert!(c > 0);

    // The newest retained copy carried the same commit, so recovery lost nothing — and the store
    // is a working store again, not merely a readable one.
    let mut s = Store::open(&dir, cfg()).unwrap();
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "recovery lost {id}");
    }
    let after = put(&mut s, "after", b"recovery");
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(s.reconstruct("after").unwrap().unwrap(), after);
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
    let parts_before = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
        .count();
    assert_eq!(parts_before, 1);

    // A read that fails for a reason OTHER than absence. A directory reads as EISDIR everywhere.
    let man = dir.join("MANIFEST");
    std::fs::remove_file(&man).unwrap();
    std::fs::create_dir(&man).unwrap();

    assert!(Store::open(&dir, cfg()).is_err(), "an unreadable manifest must refuse to open");
    let parts_after = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
        .count();
    assert_eq!(parts_after, 1, "REFUSING TO OPEN MUST NOT DELETE DATA");

    // and once the manifest is readable again the store is intact
    std::fs::remove_dir(&man).unwrap();
    std::fs::write(
        &man,
        serde_json::to_vec(&Manifest {
            parts: vec![PartRef {
                file: "part-00000001.part".into(),
                seq_lo: 1,
                seq_hi: 1,
                records: 20,
                b3: None,
            }],
            fold_seg: 0,
            fold_off: 0,
            next_seq: 1,
            fold_gen: 0,
            commit: 1,
            prev: None,
            punched: Vec::new(),
        })
        .unwrap(),
    )
    .unwrap();
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
        s.sync().unwrap();
        s.flush().unwrap();
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

#[test]
fn the_dedup_window_is_actually_released_at_every_flush() {
    // Three doc comments claimed this; nothing did it. `seal_window` was called only from a test, so
    // Tier 0 grew for the process lifetime — 266,340 pieces resident at 400k records on a real corpus.
    // Sealing was unsafe until flush learned to resolve through both tiers: the window and the part
    // being built were the same bug from two sides.
    let dir = tmp("seal");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut peak = 0usize;
    let mut want = Vec::new();
    for f in 0..25 {
        for i in 0..20 {
            let id = format!("w{f:02}-{i:02}");
            // fresh content every round, so the window cannot stay small by dedup alone
            let body =
                format!("round {f} item {i} with enough bytes to be a real piece").into_bytes();
            s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
            want.push((id, body));
        }
        peak = peak.max(s.dedup_window_len());
        s.sync().unwrap();
        s.flush().unwrap();
        assert_eq!(s.dedup_window_len(), 0, "the window must be empty immediately after a flush");
    }
    assert!(
        peak <= 20,
        "the window peaked at {peak}; it must track ONE flush interval, not 500 pieces"
    );

    // and sealing must not cost dedup — the same content re-put after 25 flushes still costs nothing
    let fold_before = std::fs::read_dir(dir.join("fold"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len())
        .sum::<u64>();
    s.put("echo", &[Span::Piece(&want[0].1)], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    let fold_after = std::fs::read_dir(dir.join("fold"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().len())
        .sum::<u64>();
    assert_eq!(fold_after, fold_before, "sealing Tier 0 must not cost dedup — Tier 1 covers it");

    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "sealing lost {id}");
    }
    assert_eq!(s.reconstruct("echo").unwrap().unwrap(), want[0].1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_reader_survives_a_writer_merging_and_sweeping_underneath_it() {
    // open_read reads the manifest and then opens the parts it names, which is not atomic. A writer
    // that commits a merge in between unlinks the replaced inputs, and the reader is left opening a
    // file that no longer exists. This is the real interleaving, not a simulated one.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc as StdArc;

    let dir = tmp("readerrace");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        // Many parts on purpose: open_read's vulnerable window is the time between reading the
        // manifest and finishing the LAST part open, so it widens with part count.
        for b in 0..60 {
            for i in 0..12 {
                let id = format!("r{b:02}-{i:02}");
                want.push((id.clone(), put(&mut s, &id, format!("body {b}/{i}").as_bytes())));
            }
            s.sync().unwrap();
            s.flush().unwrap();
        }
        assert_eq!(s.part_count(), 60);
    }

    let stop = StdArc::new(AtomicBool::new(false));
    let rdir = dir.clone();
    let rstop = stop.clone();
    let reader = std::thread::spawn(move || {
        let mut opens = 0usize;
        while !rstop.load(Ordering::Relaxed) {
            // Any error here is the bug: the store is always in a committed, readable state.
            let rs = Store::open_read(&rdir, cfg())
                .unwrap_or_else(|e| panic!("open_read failed while a merge ran: {e}"));
            let n = rs.ids().unwrap().len();
            assert_eq!(n, 720, "a reader saw {n} of 720 ids mid-merge");
            opens += 1;
        }
        opens
    });

    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        // merge repeatedly, each one committing then unlinking its inputs
        while s.part_count() > 1 {
            let n = s.part_count();
            s.merge_range(0, 2.min(n)).unwrap();
            std::thread::yield_now();
        }
    }
    stop.store(true, Ordering::Relaxed);
    let opens = reader.join().expect("the reader thread must not have panicked");
    assert!(opens > 0, "the reader never got a chance to run");

    let s = Store::open(&dir, cfg()).unwrap();
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn readers_open_a_coherent_snapshot_while_refolding() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc as StdArc;

    let dir = tmp("refoldreaderrace");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut want = Vec::new();
    for i in 0..24u32 {
        let id = format!("g{i:02}");
        let body = (0..128u32)
            .flat_map(|j| blake3::hash(&(i * 1000 + j).to_le_bytes()).as_bytes()[..16].to_vec())
            .collect::<Vec<u8>>();
        s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
        want.push((id, body));
    }
    s.sync().unwrap();
    s.flush().unwrap();

    // A reader opened before the generation swap must remain valid after the old paths are unlinked.
    let pinned = Store::open_read(&dir, cfg()).unwrap();

    let stop = StdArc::new(AtomicBool::new(false));
    let rstop = stop.clone();
    let rdir = dir.clone();
    let first = want[0].clone();
    let reader = std::thread::spawn(move || {
        let mut opens = 0usize;
        while !rstop.load(Ordering::Relaxed) {
            let rs = Store::open_read(&rdir, cfg())
                .unwrap_or_else(|e| panic!("open_read paired different refold generations: {e}"));
            assert_eq!(rs.ids().unwrap().len(), 24);
            assert_eq!(rs.reconstruct(&first.0).unwrap().unwrap(), first.1);
            opens += 1;
        }
        opens
    });

    // Repeated swaps widen the real race without test-only hooks.
    for _ in 0..6 {
        s.refold().unwrap();
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    let opens = reader.join().expect("reader thread must not panic");
    assert!(opens > 0, "the reader never overlapped a refold");

    for (id, body) in &want {
        assert_eq!(
            &pinned.reconstruct(id).unwrap().unwrap(),
            body,
            "a reader pinned before refold lost {id} after its generation was unlinked"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_manifest_naming_a_truly_absent_part_errors_rather_than_spinning() {
    // The retry that closes the reader race must be bounded: a part that is genuinely gone is a
    // corrupt store, and it has to surface as an error rather than a loop.
    let dir = tmp("absentpart");
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        put(&mut s, "a", b"x");
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let name = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .find(|n| n.ends_with(".part"))
        .unwrap();
    std::fs::remove_file(dir.join(&name)).unwrap();

    let t = std::time::Instant::now();
    assert!(Store::open_read(&dir, cfg()).is_err(), "a missing part must be an error");
    assert!(t.elapsed().as_secs() < 5, "the retry is not bounded");
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Deletion. Until now the store could only grow.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_deleted_record_is_gone_from_every_read_path() {
    let dir = tmp("delete");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let keep = put(&mut s, "keep", b"still here");
    put(&mut s, "gone", b"not for long");
    s.sync().unwrap();
    s.flush().unwrap();

    s.delete("gone").unwrap();
    // visible immediately, before any flush
    assert_eq!(s.reconstruct("gone").unwrap(), None, "a staged delete must take effect at once");
    assert_eq!(s.get("gone").unwrap(), None);
    assert!(!s.ids().unwrap().contains(&"gone".to_string()));
    assert_eq!(s.reconstruct("keep").unwrap().unwrap(), keep);

    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(s.reconstruct("gone").unwrap(), None, "and after the tombstone is committed");
    assert_eq!(s.get("gone").unwrap(), None);
    assert_eq!(s.ids().unwrap(), vec!["keep".to_string()]);

    // a lockless reader agrees
    let r = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r.reconstruct("gone").unwrap(), None);
    assert_eq!(r.ids().unwrap(), vec!["keep".to_string()]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_deletion_survives_a_crash() {
    let dir = tmp("delcrash");
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        put(&mut s, "a", b"alpha");
        let beta = put(&mut s, "b", b"beta");
        std::fs::write(dir.join("beta.expect"), &beta).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
        s.delete("a").unwrap();
        s.sync().unwrap(); // ACKed, never flushed
        drop(s);
    }
    let s = Store::open(&dir, cfg()).unwrap();
    assert_eq!(s.reconstruct("a").unwrap(), None, "a SYNCED deletion must survive a crash");
    let beta = std::fs::read(dir.join("beta.expect")).unwrap();
    assert_eq!(s.reconstruct("b").unwrap().unwrap(), beta);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_partial_merge_must_not_resurrect_a_deleted_record() {
    // THE gate. A tombstone exists to shadow older versions of its id. Dropping one while a part
    // outside the merge still holds an older version brings deleted data back.
    let dir = tmp("resurrect");
    let mut s = Store::open(&dir, cfg()).unwrap();
    put(&mut s, "victim", b"the original value");
    s.sync().unwrap();
    s.flush().unwrap(); // part 1: victim exists
    let filler = put(&mut s, "filler", b"unrelated");
    s.sync().unwrap();
    s.flush().unwrap(); // part 2
    s.delete("victim").unwrap();
    s.sync().unwrap();
    s.flush().unwrap(); // part 3: victim deleted
    assert_eq!(s.part_count(), 3);
    assert_eq!(s.reconstruct("victim").unwrap(), None);

    // Merge only the NEWER two. Part 1 still holds the original, so the tombstone must survive.
    let st = s.merge_range(1, 2).unwrap().unwrap();
    assert_eq!(st.tombstones_dropped, 0, "a partial merge must not discard tombstones");
    assert_eq!(st.tombstones_kept, 1);
    assert_eq!(s.reconstruct("victim").unwrap(), None, "DELETED DATA CAME BACK");

    // Now merge everything: nothing is left to shadow, so the tombstone can finally go.
    let st = s.merge_range(0, s.part_count()).unwrap().unwrap();
    assert_eq!(st.tombstones_dropped, 1, "a full merge should finally discard it");
    assert_eq!(s.reconstruct("victim").unwrap(), None, "and it stays gone");
    assert!(!s.ids().unwrap().contains(&"victim".to_string()));
    assert_eq!(s.reconstruct("filler").unwrap().unwrap(), filler);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn re_putting_a_deleted_id_brings_it_back() {
    let dir = tmp("undelete");
    let mut s = Store::open(&dir, cfg()).unwrap();
    put(&mut s, "x", b"first");
    s.sync().unwrap();
    s.flush().unwrap();
    s.delete("x").unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(s.reconstruct("x").unwrap(), None);

    let again = put(&mut s, "x", b"second life");
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(s.reconstruct("x").unwrap().unwrap(), again, "a put after a delete must win");
    assert!(s.ids().unwrap().contains(&"x".to_string()));

    s.merge_range(0, s.part_count()).unwrap();
    assert_eq!(s.reconstruct("x").unwrap().unwrap(), again, "and survive a full merge");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_writer_and_a_reader_never_disagree() {
    // The structural gate for the shared read core. Store layers a memtable over the committed parts;
    // ReadStore is the committed parts alone. Once a writer has flushed, the two must answer every
    // read identically — and three separate defects this session were exactly a fix landing in one of
    // these paths and not the other.
    let dir = tmp("agree");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut live: Vec<String> = Vec::new();

    for round in 0..8 {
        for i in 0..15 {
            let id = format!("a{round}-{i:02}");
            put(&mut s, &id, format!("body {round}/{i}").as_bytes());
            live.push(id);
        }
        // delete a spread of earlier ids, including some already deleted
        for k in (0..live.len()).step_by(7) {
            s.delete(&live[k].clone()).unwrap();
        }
        // and bring one back
        if round % 3 == 0 && !live.is_empty() {
            let id = live[0].clone();
            put(&mut s, &id, format!("revived at {round}").as_bytes());
        }
        s.sync().unwrap();
        s.flush().unwrap();
        if round % 4 == 3 && s.part_count() >= 2 {
            s.merge_range(0, 2).unwrap();
        }

        let r = Store::open_read(&dir, cfg()).unwrap();
        assert_eq!(s.ids().unwrap(), r.ids().unwrap(), "round {round}: ids() diverged");
        for id in &live {
            assert_eq!(
                s.reconstruct(id).unwrap(),
                r.reconstruct(id).unwrap(),
                "round {round}: reconstruct({id}) diverged between writer and reader"
            );
            assert_eq!(
                s.get(id).unwrap().map(|x| x.id),
                r.get(id).unwrap().map(|x| x.id),
                "round {round}: get({id}) diverged between writer and reader"
            );
        }
        // an id that was never written must be absent from both
        assert_eq!(s.reconstruct("never").unwrap(), None);
        assert_eq!(r.reconstruct("never").unwrap(), None);
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// The re-folding merge — the one operation that rewrites content.
// ---------------------------------------------------------------------------------------------

fn fold_gen_bytes(dir: &std::path::Path) -> u64 {
    let mut n = 0u64;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if e.path().is_dir() && (name == "fold" || name.starts_with("fold-")) {
            for f in std::fs::read_dir(e.path()).unwrap().flatten() {
                if f.file_name().to_string_lossy().ends_with(".fold") {
                    n += f.metadata().unwrap().len();
                }
            }
        }
    }
    n
}

/// A payload big enough that keeping or dropping it is unmistakable on disk.
fn blob(seed: u32) -> Vec<u8> {
    (0..60_000u32)
        .flat_map(|j| {
            blake3::hash(&(seed.wrapping_mul(7919) ^ j).to_le_bytes()).as_bytes()[..8].to_vec()
        })
        .collect()
}

#[test]
fn refold_reclaims_deleted_content_and_keeps_the_rest_byte_exact() {
    let dir = tmp("refold");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut keep = Vec::new();
    for i in 0..10u32 {
        let b = blob(i);
        s.put(
            &format!("k{i:02}"),
            &[Span::Piece(&b)],
            vec![("n".into(), AttrValue::Int(i as i64))],
        )
        .unwrap();
        keep.push((format!("k{i:02}"), b));
    }
    for i in 100..110u32 {
        let b = blob(i);
        s.put(&format!("d{i:03}"), &[Span::Piece(&b)], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    let before = fold_gen_bytes(&dir);

    for i in 100..110u32 {
        s.delete(&format!("d{i:03}")).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    // Deleting reclaims nothing on its own — that is the whole reason this operation exists.
    assert_eq!(fold_gen_bytes(&dir), before, "a delete must not touch the fold");

    let st = s.refold().unwrap();
    assert_eq!(st.tombstones_dropped, 10);
    assert_eq!(st.records_kept, 10);
    assert_eq!(st.pieces_dropped, 10, "the deleted records' content must be dropped");
    let after = fold_gen_bytes(&dir);
    // Half the content was deleted, so about half should be gone — "about", because a segment carries
    // a header and each block a frame, and asserting an exact half would be asserting the framing.
    assert!(
        after < before * 6 / 10,
        "refold kept {after} of {before} bytes; roughly half should have gone"
    );
    assert!(
        st.bytes_reclaimed() > before * 4 / 10,
        "stats claim {} reclaimed of {before}",
        st.bytes_reclaimed()
    );

    for (id, body) in &keep {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "refold corrupted {id}");
        let rec = s.get(id).unwrap().unwrap();
        assert_eq!(rec.attrs.len(), 1, "attributes must survive a refold");
    }
    for i in 100..110u32 {
        assert_eq!(s.reconstruct(&format!("d{i:03}")).unwrap(), None);
    }
    assert_eq!(s.ids().unwrap().len(), 10);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_refolded_store_reopens_and_keeps_working() {
    let dir = tmp("refoldreopen");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for i in 0..24u32 {
            let b = blob(i + 500);
            s.put(&format!("x{i:02}"), &[Span::Piece(&b)], vec![]).unwrap();
            want.push((format!("x{i:02}"), b));
            if i % 8 == 7 {
                s.sync().unwrap();
                s.flush().unwrap();
            }
        }
        s.sync().unwrap();
        s.flush().unwrap();
        for i in 0..24u32 {
            if i % 3 == 0 {
                s.delete(&format!("x{i:02}")).unwrap();
            }
        }
        s.sync().unwrap();
        s.flush().unwrap();
        s.refold().unwrap();
        drop(s);
    }
    // Exactly one fold generation must remain: the old one is swept.
    let folds: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n == "fold" || n.starts_with("fold-"))
        .collect();
    assert_eq!(folds.len(), 1, "stale fold generations must be swept: {folds:?}");

    let mut s = Store::open(&dir, cfg()).unwrap();
    for (i, (id, body)) in want.iter().enumerate() {
        if i % 3 == 0 {
            assert_eq!(s.reconstruct(id).unwrap(), None, "{id} was deleted before the refold");
        } else {
            assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "{id} did not survive reopen");
        }
    }
    // and the store still WRITES afterwards, into the new generation
    let fresh = blob(9999);
    s.put("after", &[Span::Piece(&fresh)], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(s.reconstruct("after").unwrap().unwrap(), fresh);

    let r = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r.reconstruct("after").unwrap().unwrap(), fresh);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn refold_still_dedups_against_the_new_fold() {
    // The new fold's parts carry fresh dictionaries. If Tier-1 did not follow, every subsequent write
    // of already-stored content would be appended again.
    let dir = tmp("refolddedup");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let b = blob(4242);
    s.put("orig", &[Span::Piece(&b)], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    s.refold().unwrap();

    let before = fold_gen_bytes(&dir);
    s.put("copy", &[Span::Piece(&b)], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(
        fold_gen_bytes(&dir),
        before,
        "content already in the NEW fold was stored twice — Tier-1 did not survive the refold"
    );
    assert_eq!(s.reconstruct("copy").unwrap().unwrap(), b);
    assert_eq!(s.reconstruct("orig").unwrap().unwrap(), b);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn refold_refuses_with_a_dirty_memtable() {
    let dir = tmp("refolddirty");
    let mut s = Store::open(&dir, cfg()).unwrap();
    put(&mut s, "a", b"x");
    s.sync().unwrap();
    s.flush().unwrap();
    put(&mut s, "b", b"y"); // staged, references the OLD fold
    assert!(s.refold().is_err(), "refolding under a dirty memtable must refuse");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_crashed_refold_leaves_the_store_exactly_as_it_was() {
    // A refold writes a new fold generation and new parts BEFORE the manifest names either. A crash in
    // that window must leave nothing but orphans, and the store must open on the old generation as
    // though the refold had never started.
    let dir = tmp("refoldcrash");
    let mut want = Vec::new();
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        for i in 0..12u32 {
            let b = blob(i + 300);
            s.put(&format!("c{i:02}"), &[Span::Piece(&b)], vec![]).unwrap();
            want.push((format!("c{i:02}"), b));
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let manifest_before = std::fs::read(dir.join("MANIFEST")).unwrap();

    // Exactly what a refold leaves behind when it dies before committing: a populated next-generation
    // fold, and part files the manifest does not name.
    let ghost = dir.join("fold-0001");
    std::fs::create_dir_all(&ghost).unwrap();
    std::fs::write(ghost.join("000000.fold"), vec![7u8; 4096]).unwrap();
    std::fs::write(dir.join("part-r0001-00000001-00000001.part"), vec![9u8; 2048]).unwrap();

    let s = Store::open(&dir, cfg()).unwrap();
    assert!(!ghost.exists(), "an uncommitted fold generation must be swept");
    assert!(
        !dir.join("part-r0001-00000001-00000001.part").exists(),
        "parts the manifest does not name must be swept"
    );
    assert_eq!(
        std::fs::read(dir.join("MANIFEST")).unwrap(),
        manifest_before,
        "the committed state must be untouched"
    );
    for (id, body) in &want {
        assert_eq!(
            &s.reconstruct(id).unwrap().unwrap(),
            body,
            "{id} did not survive a crashed refold"
        );
    }
    // and a refold started afresh still works
    drop(s);
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.refold().unwrap();
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn auto_compact_settles_the_store_and_its_deletes() {
    let dir = tmp("autocompact");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut want = Vec::new();
    for f in 0..turndb::store::Store::AUTO_COMPACT_K {
        for i in 0..5 {
            let id = format!("a{f:02}-{i}");
            want.push((id.clone(), put(&mut s, &id, format!("v{f}{i}").as_bytes())));
        }
        s.sync().unwrap();
        s.flush().unwrap();
        s.auto_compact().unwrap();
    }
    s.delete("a00-0").unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    want.retain(|(id, _)| id != "a00-0");
    // drive to the threshold so the delete's tombstone rides through a TOTAL merge
    while s.part_count() < turndb::store::Store::AUTO_COMPACT_K {
        let n = s.part_count();
        put(&mut s, &format!("pad-{n}"), b"p");
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let stats = s.auto_compact().unwrap().expect("at threshold, the policy must merge");
    assert_eq!(s.part_count(), 1, "a total merge leaves one part");
    assert_eq!(stats.tombstones_dropped, 1, "a total merge SETTLES deletes");
    for (id, body) in &want {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body);
    }
    assert!(s.reconstruct("a00-0").unwrap().is_none(), "the delete holds after settlement");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn erase_ids_leaves_no_content_no_metadata_and_no_snapshot_path_back() {
    let dir = tmp("erase");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let mut keep = Vec::new();
    for f in 0..3 {
        for i in 0..8 {
            let id = format!("e{f}-{i}");
            let body = put(
                &mut s,
                &id,
                format!("unique payload {f}/{i} {}", "q".repeat(200 + i)).as_bytes(),
            );
            if !(f == 0 && i < 2) {
                keep.push((id, body));
            }
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let before = s.fold().disk_bytes();

    let stats = s.erase_ids(&["e0-0".into(), "e0-1".into(), "never-existed".into()]).unwrap();
    assert_eq!(stats.tombstoned, 2);
    assert_eq!(stats.absent, 1);
    let refold = stats.refold.expect("content existed, so the fold must have been rewritten");
    assert!(refold.pieces_dropped > 0, "the erased records' unique pieces must be dropped");
    assert!(s.fold().disk_bytes() < before, "bytes must actually leave the disk");

    // content gone, everything else byte-exact
    assert!(s.reconstruct("e0-0").unwrap().is_none());
    assert!(s.reconstruct("e0-1").unwrap().is_none());
    for (id, body) in &keep {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "{id} damaged by erasure");
    }

    // METADATA gone: no part row carries the erased ids, tombstones included — the parts were
    // rebuilt, not merely shadowed
    for p in s.parts() {
        assert!(p.find("e0-0").unwrap().is_none(), "an erased id must not remain as any row");
        assert_eq!(p.tombstones().unwrap().len(), 0, "settlement must leave no tombstones");
    }

    // and TIME TRAVEL cannot resurrect it: the retained log was purged to the erasure's commit
    let snaps = turndb::store::retained_commits(&dir);
    assert_eq!(snaps.len(), 1, "erasure must purge every snapshot that could still serve the data");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_manifest_chain_links_and_pins_and_notices_tampering() {
    let dir = tmp("chain");
    let mut s = Store::open(&dir, cfg()).unwrap();
    for f in 0..3 {
        put(&mut s, &format!("c{f}"), format!("body {f}").as_bytes());
        s.sync().unwrap();
        s.flush().unwrap();
    }
    s.merge_range(0, 2).unwrap().unwrap();
    drop(s);

    let report = turndb::store::verify_chain(&dir).unwrap();
    assert!(report.links >= 3, "commits must chain: {report:?}");
    assert!(report.part_digests >= 4, "every named part must be pinned: {report:?}");
    assert_eq!(report.undigested, 0, "a fresh store has no undigested parts");

    // tamper INSIDE a part (past the footer's own reach): the manifest pin must notice
    let part = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .unwrap();
    let mut b = std::fs::read(&part).unwrap();
    b[2] ^= 0xFF;
    std::fs::write(&part, &b).unwrap();
    assert!(turndb::store::verify_chain(&dir).is_err(), "a drifted part must break verification");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn punching_reclaims_erased_bytes_in_place_without_moving_anything() {
    let dir = tmp("punch");
    let mut s = Store::open(&dir, FoldCfg { block_target: 256 * 1024, ..cfg() }).unwrap();
    // INCOMPRESSIBLE bodies: hole punching frees whole filesystem blocks, so a fold that
    // compresses into a single 4 KiB block has nothing observable to free.
    let noise = |seed: u64, len: usize| -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut h = blake3::hash(&seed.to_le_bytes());
        while out.len() < len {
            out.extend_from_slice(h.as_bytes());
            h = blake3::hash(h.as_bytes());
        }
        out.truncate(len);
        out
    };
    let mut keep = Vec::new();
    for f in 0..4u64 {
        for i in 0..6u64 {
            let id = format!("p{f}-{i}");
            let want = put(&mut s, &id, &noise(f * 100 + i, 64 * 1024));
            if f > 0 {
                keep.push((id, want));
            }
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    // delete and SETTLE the first flush's records, so their blocks become unreachable
    for i in 0..6 {
        s.delete(&format!("p0-{i}")).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    s.merge_range(0, s.part_count()).unwrap().unwrap();

    let before = allocated_bytes(&dir.join("fold"));
    let stats = s.punch_unreferenced().unwrap();
    assert!(stats.blocks_punched > 0, "dead blocks must be punched: {stats:?}");
    assert!(!s.manifest().punched.is_empty(), "the manifest must NAME what was punched");

    // bytes actually left the disk, and the file LENGTHS did not change (offsets are stable)
    let after = allocated_bytes(&dir.join("fold"));
    assert!(after < before, "punching must deallocate: {before} -> {after}");

    // everything still live reads byte-exact, through the same unmoved offsets
    for (id, body) in &keep {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "{id} damaged by punching");
    }
    // and a reopened store agrees
    drop(s);
    let s = Store::open(&dir, cfg()).unwrap();
    for (id, body) in &keep {
        assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "{id} damaged across reopen");
    }
    assert!(!s.manifest().punched.is_empty(), "the punched list must survive reopen");
    std::fs::remove_dir_all(&dir).ok();
}

/// Blocks actually allocated on disk, in bytes — what punching changes (file LENGTH does not).
fn allocated_bytes(d: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir(d)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".fold"))
        .map(|e| e.metadata().unwrap().blocks() * 512)
        .sum()
}

#[test]
fn the_writer_reads_its_own_unflushed_writes_and_a_reader_does_not() {
    // The visibility contract a live UI depends on. A single-process server that holds the writer
    // can serve a record the instant it is written — no flush, no commit, no wait. A SEPARATE
    // reader sees only committed state, which is what makes it safe to run beside the writer.
    // Both halves matter: the first is what makes "live" possible, the second is why readers
    // never see a half-written world.
    let dir = tmp("livewrite");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let committed = put(&mut s, "before", b"flushed content");
    s.sync().unwrap();
    s.flush().unwrap();

    // written, ACKed, but NOT flushed — no part, no manifest commit
    let staged = put(&mut s, "live", b"content that has not been flushed");
    s.sync().unwrap();

    // the writer serves it immediately, byte-exact
    assert_eq!(s.reconstruct("live").unwrap().unwrap(), staged, "writer must see its own write");
    assert!(s.ids().unwrap().contains(&"live".to_string()), "and list it");
    assert_eq!(s.part_count(), 1, "still only the earlier part — nothing was committed");

    // a concurrent reader sees the committed world only
    let r = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r.reconstruct("before").unwrap().unwrap(), committed);
    assert!(r.reconstruct("live").unwrap().is_none(), "a reader must not see uncommitted records");

    // ... until the flush, which is the visibility boundary
    s.flush().unwrap();
    let r = Store::open_read(&dir, cfg()).unwrap();
    assert_eq!(r.reconstruct("live").unwrap().unwrap(), staged, "flush publishes it");
    std::fs::remove_dir_all(&dir).ok();
}

/// A page must be FULL whenever enough live ids exist — not merely free of deleted ones.
///
/// The committed scan is bounded by the page limit, so ids shadowed by a staged deletion used to
/// leave a hole nothing backfilled: the page came back short while live rows sat just past the
/// limit, and nothing reported that it was short. Asserting cardinality is the point of this test;
/// the sibling test above deletes one id and asserts its absence, which this defect survived.
#[test]
fn scan_ids_fills_the_page_past_staged_deletions() {
    let dir = tmp("scanidsfill");
    let mut s = Store::open(&dir, cfg()).unwrap();
    for id in ["a", "b", "c", "d", "e"] {
        put(&mut s, id, b"x");
    }
    s.sync().unwrap();
    s.flush().unwrap();

    // Staged, deliberately NOT flushed: the deletions live in the memtable, which is where they
    // shadow committed ids without removing them from the committed scan's own count.
    s.delete("a").unwrap();
    s.delete("b").unwrap();
    s.sync().unwrap();

    assert_eq!(
        s.scan_ids(None, None, 3, false).unwrap(),
        vec!["c", "d", "e"],
        "a full page of live ids exists past the deletions and must be returned"
    );

    // Deletions at the other end, against a reverse scan — a fix that over-fetches in only one
    // direction passes the forward case and fails here.
    let dir2 = tmp("scanidsfillrev");
    let mut s2 = Store::open(&dir2, cfg()).unwrap();
    for id in ["a", "b", "c", "d", "e"] {
        put(&mut s2, id, b"x");
    }
    s2.sync().unwrap();
    s2.flush().unwrap();
    s2.delete("e").unwrap();
    s2.delete("d").unwrap();
    s2.sync().unwrap();
    assert_eq!(
        s2.scan_ids(None, None, 3, true).unwrap(),
        vec!["c", "b", "a"],
        "reverse pages must fill past deletions at the high end"
    );

    // Bounded range, deletions inside it, and a live id past the limit that must be pulled in.
    assert_eq!(
        s.scan_ids(Some("a"), Some("f"), 2, false).unwrap(),
        vec!["c", "d"],
        "a bounded range fills to its limit too"
    );

    // The page is short only when the store genuinely has fewer live ids than asked for.
    assert_eq!(
        s.scan_ids(None, None, 10, false).unwrap(),
        vec!["c", "d", "e"],
        "asking for more than exist returns what exists"
    );

    // A staged PUT sorting past the committed candidate window, under a reverse scan: the merge
    // must reorder it into the page rather than let the committed slice decide the answer. Called
    // out because over-fetching by the deletion count reasons about deletions only, and a staged
    // put is the case that reasoning does not cover.
    put(&mut s, "z", b"x");
    s.sync().unwrap();
    assert_eq!(
        s.scan_ids(None, None, 3, true).unwrap(),
        vec!["z", "e", "d"],
        "an unflushed put beyond the committed window must still head a reverse page"
    );

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&dir2).ok();
}

/// An inverted range must be REFUSED, not panic.
///
/// `BTreeMap::range` traps on `start > end`. Through the WASM binding that trap crossed as
/// `RuntimeError: unreachable` and left the handle poisoned — every later call on that store failed
/// with `RefCell already borrowed`. So one reversed argument pair, both strings perfectly
/// well-formed, permanently killed the store. The corruption storm already holds every on-disk
/// parser to "errors, never panics"; this is that same standard reaching API arguments, which it
/// had not.
#[test]
fn an_inverted_scan_range_is_refused_rather_than_panicking() {
    let dir = tmp("scanidsinverted");
    let mut s = Store::open(&dir, cfg()).unwrap();
    for id in ["a", "b", "c"] {
        put(&mut s, id, b"x");
    }
    s.sync().unwrap();
    s.flush().unwrap();

    assert!(s.scan_ids(Some("z"), Some("a"), 10, false).is_err(), "writer path must refuse");
    // ...and the store still works afterwards, which is the half that was actually lost.
    assert_eq!(s.scan_ids(None, None, 10, false).unwrap(), vec!["a", "b", "c"]);

    let r = Store::open_read(&dir, cfg()).unwrap();
    assert!(r.scan_ids(Some("z"), Some("a"), 10, false).is_err(), "reader path must refuse too");
    assert_eq!(r.scan_ids(None, None, 10, false).unwrap().len(), 3);

    // Equal bounds are a legitimately EMPTY half-open range, not an error.
    assert_eq!(s.scan_ids(Some("b"), Some("b"), 10, false).unwrap(), Vec::<String>::new());

    // An astral pair, because the guard must order by UTF-8 bytes like the store. Rust `str` Ord is
    // byte order so this holds naturally — the test exists so a future rewrite in a language that
    // compares UTF-16 code units cannot quietly invert it: the astral bound sorts ABOVE the BMP one
    // in UTF-8 and BELOW it in UTF-16.
    let astral = "a\u{10000}";
    let bmp = "a\u{FFFF}";
    assert!(
        s.scan_ids(Some(astral), Some(bmp), 10, false).is_err(),
        "astral-vs-BMP inversion must be refused under UTF-8 ordering"
    );
    assert!(s.scan_ids(Some(bmp), Some(astral), 10, false).is_ok(), "and its reverse is valid");

    std::fs::remove_dir_all(&dir).ok();
}

/// Seamus's adversarial fixture, kept because it is the case the author did not think of.
///
/// Six interactions at once, where the tests above exercise them separately: a bounded range, two
/// deletions of committed ids inside it, a deletion of an id that is in range but was never
/// committed, a deletion outside the range entirely (which must not be counted against the
/// candidate budget), a staged put landing mid-range, and both scan directions. A candidate budget
/// that miscounts any of those returns a short or wrong page here while passing every other test.
#[test]
fn scan_ids_mixed_overlay_fills_bounded_pages_in_both_directions() {
    let dir = tmp("scanidsmixedoverlay");
    let mut s = Store::open(&dir, cfg()).unwrap();
    for id in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        put(&mut s, id, b"x");
    }
    s.sync().unwrap();
    s.flush().unwrap();

    s.delete("c").unwrap(); // committed, in range
    s.delete("f").unwrap(); // committed, in range
    s.delete("cc").unwrap(); // in range, never committed — must not consume budget wrongly
    s.delete("z").unwrap(); // outside the range — must not be counted at all
    put(&mut s, "d0", b"x"); // staged put landing mid-range
    s.sync().unwrap();

    // Live in [b,h) is b, d, d0, e, g.
    assert_eq!(
        s.scan_ids(Some("b"), Some("h"), 4, false).unwrap(),
        vec!["b", "d", "d0", "e"],
        "forward page over a bounded range with mixed staged state"
    );
    assert_eq!(
        s.scan_ids(Some("b"), Some("h"), 4, true).unwrap(),
        vec!["g", "e", "d0", "d"],
        "reverse page over the same state"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn scan_ids_pages_a_range_and_honours_every_visibility_rule() {
    let dir = tmp("scanids");
    let mut s = Store::open(&dir, cfg()).unwrap();
    // ids shaped like the integration's: member/zero-padded-ts/rid — so lexicographic order IS
    // member-then-time order, which is what makes a page one range scan.
    for m in ["alice", "bob"] {
        for t in 0..30u64 {
            put(&mut s, &format!("{m}/{:013}/r{t}", 1785000000000u64 + t), b"x");
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }

    // a member's page, newest first
    let page = s.scan_ids(Some("alice/"), Some("alice0"), 10, true).unwrap();
    assert_eq!(page.len(), 10);
    assert!(page.iter().all(|id| id.starts_with("alice/")), "must not cross into bob: {page:?}");
    assert_eq!(page[0], "alice/1785000000029/r29", "reverse must start at the newest");
    assert!(page[0] > page[9], "reverse must descend");

    // forward paging, and a time-bounded window
    let first = s.scan_ids(Some("alice/"), Some("alice0"), 5, false).unwrap();
    assert_eq!(first[0], "alice/1785000000000/r0");
    let window =
        s.scan_ids(Some("alice/1785000000010"), Some("alice/1785000000020"), 100, false).unwrap();
    assert_eq!(window.len(), 10, "half-open range: 10..20");

    // spans parts, and the whole store when unbounded
    assert_eq!(s.scan_ids(None, None, 1000, false).unwrap().len(), 60);
    assert_eq!(s.scan_ids(None, None, 3, false).unwrap().len(), 3, "limit is respected");

    // UNCOMMITTED writes are visible to the writer — the live-backfill property
    put(&mut s, "alice/1785000000099/live", b"unflushed");
    s.sync().unwrap();
    let page = s.scan_ids(Some("alice/"), Some("alice0"), 3, true).unwrap();
    assert_eq!(page[0], "alice/1785000000099/live", "writer must page its own unflushed write");
    // ... and not to a separate reader
    let r = Store::open_read(&dir, cfg()).unwrap();
    let rpage = r.scan_ids(Some("alice/"), Some("alice0"), 3, true).unwrap();
    assert_eq!(rpage[0], "alice/1785000000029/r29", "a reader sees committed state only");

    // deletions drop out of the page, staged and settled alike
    s.delete("alice/1785000000029/r29").unwrap();
    s.sync().unwrap();
    let page = s.scan_ids(Some("alice/"), Some("alice0"), 3, true).unwrap();
    assert!(!page.contains(&"alice/1785000000029/r29".to_string()), "staged delete must vanish");
    s.flush().unwrap();
    s.merge_range(0, s.part_count()).unwrap();
    let page = s.scan_ids(Some("alice/"), Some("alice0"), 3, true).unwrap();
    assert!(
        !page.contains(&"alice/1785000000029/r29".to_string()),
        "settled delete must stay gone"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn health_is_cheap_complete_and_tracks_publication() {
    let dir = tmp("health");
    let config = cfg();
    let mut s = Store::open(&dir, config).unwrap();
    let empty = s.health();
    assert_eq!(empty.parts, 0);
    assert_eq!(empty.part_rows, 0);
    assert_eq!(empty.memtable_entries, 0);
    assert_eq!(empty.wal_bytes, 0);
    assert!(empty.part_cache_budget > 0);

    put(&mut s, "health/1", b"payload that becomes one folded piece");
    let staged = s.health();
    assert_eq!(staged.memtable_entries, 1);
    assert!(staged.memtable_bytes > 0);
    assert!(staged.wal_bytes > 0);
    assert_eq!(staged.dedup_window_entries, 3);
    assert_eq!(staged.parts, 0);

    s.sync().unwrap();
    s.flush().unwrap();
    let published = s.health();
    assert!(published.commit > empty.commit);
    assert_eq!(published.parts, 1);
    assert_eq!(published.part_rows, 1);
    assert_eq!(published.memtable_entries, 0);
    assert_eq!(published.memtable_bytes, 0);
    assert_eq!(published.wal_bytes, 0);
    assert_eq!(published.dedup_window_entries, 0);
    assert_eq!(published.retained_commits, 1);
    assert!(published.fold_disk_bytes > 0);
    assert_eq!(published.fold_segments, 1);
    std::fs::remove_dir_all(&dir).ok();
}
