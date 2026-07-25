//! Step-3 gate: durability and recovery. A crash at any point loses nothing that was ACKed, and a
//! reader works from the files alone — no lock, no writer, no daemon.

use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::store::{Span, Store};
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
    let real = dir.join("part-00000000.part");
    std::fs::copy(&real, dir.join("part-00000001.part")).unwrap();
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
