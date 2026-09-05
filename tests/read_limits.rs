use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use turndb::error::{classify, ErrorClass};
use turndb::fold::{Fold, FoldCfg};
use turndb::part::{self, Part};
use turndb::read_limits::{ReadAdmissionError, ReadLimits};
use turndb::store::{Batch, Span, Store, StoreOptions};
use turndb::{AttrValue, Record};

struct ScopedDir(PathBuf);

impl ScopedDir {
    fn new(label: &str) -> ScopedDir {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir()
            .join(format!("turndb-read-limits-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        ScopedDir(path)
    }
}

impl std::ops::Deref for ScopedDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn limits(stored: u64, decoded: u64) -> ReadLimits {
    ReadLimits {
        max_stored_frame_bytes: stored,
        max_decoded_frame_bytes: decoded,
        ..ReadLimits::default()
    }
}

fn binary_record(bytes: usize) -> Record {
    let value = (0..bytes).map(|i| ((i * 131 + i / 7) & 0xff) as u8).collect();
    Record {
        id: "record".into(),
        contents: Vec::new(),
        attrs: vec![("binary".into(), AttrValue::Bytes(value))],
    }
}

#[test]
fn part_toc_and_selected_sections_are_admitted_before_decode() {
    let dir = ScopedDir::new("part-read");
    let path = dir.join("part.part");
    let record = binary_record(4096);
    part::build(&path, &[record], 1, 1, 3, |_| None).unwrap();

    let toc_error = match Part::open_with_limits(&path, limits(1, 1)) {
        Ok(_) => panic!("one byte cannot admit the part TOC"),
        Err(error) => error,
    };
    assert_eq!(classify(&toc_error), ErrorClass::ResourceExhausted);
    assert!(matches!(
        toc_error.downcast_ref::<ReadAdmissionError>(),
        Some(ReadAdmissionError::StoredFrameTooLarge { frame, .. }) if frame == "part TOC"
    ));

    // The TOC itself fits, and opening remains metadata-only. The binary dictionary is refused only
    // when the selected record asks the part to touch it.
    let part = Part::open_with_limits(&path, limits(16 << 10, 512)).unwrap();
    assert_eq!(part.id(0).unwrap(), "record");
    let section_error = part.record(0).unwrap_err();
    assert_eq!(classify(&section_error), ErrorClass::ResourceExhausted);
    assert!(matches!(
        section_error.downcast_ref::<ReadAdmissionError>(),
        Some(ReadAdmissionError::DecodedFrameTooLarge { frame, .. })
            if frame.contains("part section")
    ));
}

#[test]
fn part_writer_refuses_an_unreopenable_section_before_footer_publication() {
    let dir = ScopedDir::new("part-write");
    let path = dir.join("part.part");
    let record = binary_record(4096);
    let error = part::build_full_with_limits(
        &path,
        &[record],
        &[],
        1,
        1,
        3,
        |_| None,
        &Default::default(),
        limits(16 << 10, 512),
    )
    .unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert!(Part::open(&path).is_err(), "a refused builder must not land a complete footer");
}

#[test]
fn streaming_compaction_refuses_an_oversized_output_section_before_publication() {
    let dir = ScopedDir::new("merge-write");
    let strict = limits(16 << 10, 512);
    let mut records = Vec::new();
    for (seq, fill) in [(1, 0x17), (2, 0xe3)] {
        let path = dir.join(format!("part-{seq}.part"));
        let record = Record {
            id: format!("record-{seq}"),
            contents: Vec::new(),
            attrs: vec![("binary".into(), AttrValue::Bytes(vec![fill; 300]))],
        };
        part::build(&path, &[record], seq, seq, 3, |_| None).unwrap();
        records.push(Arc::new(Part::open_with_limits(&path, strict).unwrap()));
    }

    let output = dir.join("merged.part");
    let error = part::merge::merge_opts_with_control_and_limits(
        &output,
        &records,
        3,
        false,
        &Default::default(),
        strict,
    )
    .unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert!(Part::open(&output).is_err(), "a refused merge must not publish a footer");
}

#[test]
fn strict_fold_profile_splits_for_progress_and_refuses_one_oversized_piece_before_mutation() {
    let dir = ScopedDir::new("fold-progress");
    let cfg = FoldCfg { block_target: 1024, ..FoldCfg::default() };
    let strict = limits(64, 64);
    let mut fold = Fold::open_with_limits(&dir, cfg, strict).unwrap();

    let a = vec![0x11; 40];
    let b = vec![0x22; 40];
    let pa = fold.put(&a).unwrap();
    let pb = fold.put(&b).unwrap();
    assert_ne!(pa.loc.block_id, pb.loc.block_id, "the read ceiling must become a seal target");

    let before = fold.tail();
    let error = fold.put(&[0x33; 65]).unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert_eq!(fold.tail(), before, "an indivisible refusal must precede persisted mutation");
    fold.sync().unwrap();
    drop(fold);

    let reopened = Fold::open_read_with_limits(&dir, cfg, &[], strict).unwrap();
    assert_eq!(reopened.read_verified(pa.loc, pa.hash).unwrap(), a);
    assert_eq!(reopened.read_verified(pb.loc, pb.hash).unwrap(), b);
}

#[test]
fn a_late_oversized_batch_piece_is_refused_before_any_fold_or_wal_mutation() {
    let dir = ScopedDir::new("batch-preflight");
    let strict = limits(64, 64);
    let mut store = Store::open_file_with_options(
        &store_file(&dir),
        StoreOptions {
            fold: FoldCfg { block_target: 1024, ..FoldCfg::default() },
            read_limits: strict,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let before = store.health();
    let mut batch = Batch::new();
    batch.put("first", &[Span::Piece(&[0x11; 40])], vec![]);
    batch.put("second", &[Span::Piece(&[0x22; 65])], vec![]);
    let error = store.apply(batch).unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert_eq!(store.health(), before);
    assert!(store.get("first").unwrap().is_none());
}

#[test]
fn aggregate_wal_frame_admission_precedes_fold_mutation() {
    let dir = ScopedDir::new("wal-preflight");
    let strict = limits(192, 192);
    let mut store = Store::open_file_with_options(
        &store_file(&dir),
        StoreOptions { read_limits: strict, ..StoreOptions::default() },
    )
    .unwrap();
    let before = store.health();
    let error = store
        .put("aggregate", &[Span::Piece(&[0x31; 80]), Span::Piece(&[0x72; 80])], vec![])
        .unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert!(error.to_string().contains("new WAL frame"));
    assert_eq!(store.health(), before);
}

#[test]
fn strict_tail_scan_refuses_without_truncating_valid_large_frames() {
    let dir = ScopedDir::new("fold-tail");
    let cfg = FoldCfg { block_target: 128, ..FoldCfg::default() };
    let payload: Vec<u8> = (0..128).map(|i| (i * 53) as u8).collect();
    let mut fold = Fold::open(&dir, cfg).unwrap();
    let put = fold.put(&payload).unwrap();
    fold.sync().unwrap();
    drop(fold);

    let segment = dir.join("seg-00000000.fold");
    let before = std::fs::metadata(&segment).unwrap().len();
    let error = match Fold::open_with_limits(&dir, cfg, limits(64, 64)) {
        Ok(_) => panic!("strict writer open must refuse the valid larger block"),
        Err(error) => error,
    };
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert_eq!(std::fs::metadata(&segment).unwrap().len(), before);

    let reopened = Fold::open_read(&dir, cfg).unwrap();
    assert_eq!(reopened.read_verified(put.loc, put.hash).unwrap(), payload);
}

#[test]
fn a_budget_refusal_never_calls_the_backup_invalid() {
    let dir = ScopedDir::new("backup-classification");
    let source = dir.join("source");
    let artifact = dir.join("backup.turndb");
    let mut store = Store::open_file(&store_file(&source), FoldCfg::default()).unwrap();
    store.put("large", &[Span::Piece(&[0x5a; 256])], vec![]).unwrap();
    store.sync().unwrap();
    store.backup(&artifact).unwrap();
    drop(store);

    // Under a starved frame budget the backup refuses as exhaustion — a statement about
    // the BUDGET, and it must classify as one.
    let error = turndb::store::open_read_container_with_limits(
        &artifact,
        FoldCfg::default(),
        limits(64, 64),
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);

    // The artifact was never the problem: ordinary limits open it and read back byte-exact.
    let rs = turndb::store::open_read_container(&artifact, FoldCfg::default()).unwrap();
    assert_eq!(rs.reconstruct("large").unwrap().unwrap(), vec![0x5a; 256]);
}

/// Build the suite's single-file store inside its cleanup directory.
fn store_file(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).ok();
    dir.join("s.turndb")
}
