use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use turndb::fold::FoldCfg;
use turndb::store::{Batch, ContentSpans, Span, Store, WriteAdmissionError, WriteLimits};
use turndb::AttrValue;

struct ScopedDir(PathBuf);

impl Deref for ScopedDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn tmp(label: &str) -> ScopedDir {
    ScopedDir(std::env::temp_dir().join(format!(
        "turndb-write-limits-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )))
}

fn limits(record: u64, batch: u64) -> WriteLimits {
    WriteLimits {
        max_record_bytes: record,
        max_batch_bytes: batch,
        max_batch_records: 16,
        max_identifier_bytes: 64,
    }
}

#[test]
fn record_limit_is_an_exact_inclusive_framed_wal_boundary() {
    let dir = tmp("record-boundary");
    // Revision-4 record with id "x", content name "body", one literal op, no attrs or novel pieces:
    // 63 bytes of framing/metadata plus the literal bytes.
    let mut store =
        Store::open_file_with_limits(&store_file(&dir), FoldCfg::default(), limits(67, 1_000))
            .unwrap();
    store.put("x", &[Span::Lit(b"1234")], vec![]).unwrap();
    let before = store.health();

    let error = store.put("y", &[Span::Lit(b"12345")], vec![]).unwrap_err();
    assert_eq!(
        error.downcast_ref::<WriteAdmissionError>(),
        Some(&WriteAdmissionError::RecordTooLarge { item: None, actual: 68, allowed: 67 })
    );
    assert_eq!(store.health(), before, "a refused record changes no engine state");
    assert!(store.get("y").unwrap().is_none());
}

#[test]
fn worst_case_measurement_does_not_depend_on_dedup_state() {
    let dir = tmp("dedup-independent");
    let bytes = b"same";
    {
        let mut store = Store::open_file(&store_file(&dir), FoldCfg::default()).unwrap();
        store.put("x", &[Span::Piece(bytes)], vec![]).unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
    }

    // The all-novel upper bound is 132 bytes. It remains the admission size even though the piece
    // is already durable and this particular write would carry no novel bytes.
    let mut store =
        Store::open_file_with_limits(&store_file(&dir), FoldCfg::default(), limits(131, 1_000))
            .unwrap();
    let error = store.put("y", &[Span::Piece(bytes)], vec![]).unwrap_err();
    assert_eq!(
        error.downcast_ref::<WriteAdmissionError>(),
        Some(&WriteAdmissionError::RecordTooLarge { item: None, actual: 132, allowed: 131 })
    );
}

#[test]
fn atomic_batch_limit_includes_members_and_commit_marker() {
    let dir = tmp("batch-boundary");
    // A one-byte tombstone frame is 18 bytes. Two members plus an 18-byte commit-marker frame = 54.
    let mut store =
        Store::open_file_with_limits(&store_file(&dir), FoldCfg::default(), limits(100, 54))
            .unwrap();
    let mut accepted = Batch::new();
    accepted.delete("a");
    accepted.delete("b");
    store.apply(accepted).unwrap();
    let before = store.health();

    let mut rejected = Batch::new();
    rejected.delete("c");
    rejected.delete("d");
    rejected.delete("e");
    let error = store.apply(rejected).unwrap_err();
    assert_eq!(
        error.downcast_ref::<WriteAdmissionError>(),
        Some(&WriteAdmissionError::BatchTooLarge { actual: 72, allowed: 54 })
    );
    assert_eq!(store.health(), before, "batch refusal must be atomic in memory and WAL");
    assert!(store.get("c").unwrap().is_none());
}

#[test]
fn batch_record_count_is_inclusive_and_checked_before_byte_work() {
    let dir = tmp("batch-count");
    let mut write_limits = limits(100, 1_000);
    write_limits.max_batch_records = 2;
    let mut store =
        Store::open_file_with_limits(&store_file(&dir), FoldCfg::default(), write_limits).unwrap();
    let mut accepted = Batch::new();
    accepted.delete("a");
    accepted.delete("b");
    store.apply(accepted).unwrap();

    let before = store.health();
    let mut rejected = Batch::new();
    rejected.delete("c");
    rejected.delete("d");
    rejected.delete("e");
    let error = store.apply(rejected).unwrap_err();
    assert_eq!(
        error.downcast_ref::<WriteAdmissionError>(),
        Some(&WriteAdmissionError::TooManyBatchRecords { actual: 3, allowed: 2 })
    );
    assert_eq!(store.health(), before);
}

#[test]
fn a_late_oversized_batch_item_is_identified_before_any_fold_work() {
    let dir = tmp("batch-item");
    let mut store =
        Store::open_file_with_limits(&store_file(&dir), FoldCfg::default(), limits(67, 1_000))
            .unwrap();
    let before = store.health();
    let mut batch = Batch::new();
    batch.put("a", &[Span::Lit(b"1234")], vec![]);
    batch.put("b", &[Span::Piece(b"too large as a worst-case novel piece")], vec![]);
    let error = store.apply(batch).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<WriteAdmissionError>(),
        Some(WriteAdmissionError::RecordTooLarge { item: Some(1), .. })
    ));
    assert_eq!(store.health(), before);
    assert!(store.get("a").unwrap().is_none());
}

#[test]
fn identifiers_are_utf8_byte_bounded_without_reserving_a_vocabulary() {
    let dir = tmp("identifiers");
    let mut write_limits = limits(1_000, 2_000);
    write_limits.max_identifier_bytes = 4;
    let mut store =
        Store::open_file_with_limits(&store_file(&dir), FoldCfg::default(), write_limits).unwrap();
    store.put("éé", &[Span::Lit(b"")], vec![("éé".into(), AttrValue::Null)]).unwrap();

    let error = store.put("ééa", &[Span::Lit(b"")], vec![]).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<WriteAdmissionError>(),
        Some(WriteAdmissionError::IdentifierTooLong { kind: "record id", actual: 5, .. })
    ));
    let content = [ContentSpans::new("ééa", vec![Span::Lit(b"")])];
    let error = store.put_record("z", &content, vec![]).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<WriteAdmissionError>(),
        Some(WriteAdmissionError::IdentifierTooLong { kind: "content name", actual: 5, .. })
    ));
    let error =
        store.put("z", &[Span::Lit(b"")], vec![(String::new(), AttrValue::Null)]).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<WriteAdmissionError>(),
        Some(WriteAdmissionError::EmptyIdentifier { kind: "attribute name", .. })
    ));
}

#[test]
fn invalid_policy_is_refused_before_the_store_file_is_created() {
    let dir = tmp("invalid-policy");
    let file = store_file(&dir);
    let write_limits = WriteLimits { max_batch_records: 0, ..WriteLimits::default() };
    let error =
        Store::open_file_with_limits(&file, FoldCfg::default(), write_limits).err().unwrap();
    assert!(matches!(
        error.downcast_ref::<WriteAdmissionError>(),
        Some(WriteAdmissionError::InvalidLimits(_))
    ));
    assert!(!file.exists(), "a refused policy must create nothing");
    std::fs::remove_dir_all(&*dir).ok();
}

/// The migrated suites build single-file stores inside their temp directories: the parent is
/// ensured, the store is one file within it, and every cleanup keeps operating on the directory.
fn store_file(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).ok();
    dir.join("s.turndb")
}
