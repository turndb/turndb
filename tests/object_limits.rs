use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use turndb::error::{classify, ErrorClass};
use turndb::fold::{block, segment, Fold, FoldCfg};
use turndb::read_limits::{ReadAdmissionError, ReadLimits};
use turndb::store::{Span, Store, StoreOptions};

struct ScopedDir(PathBuf);

impl ScopedDir {
    fn new(label: &str) -> ScopedDir {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir()
            .join(format!("turndb-object-limits-{label}-{}-{nonce}", std::process::id()));
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

fn with_counts(directory: u64, wal: u64, fold: u64) -> ReadLimits {
    ReadLimits {
        max_directory_entries: directory,
        max_wal_frames: wal,
        max_fold_blocks: fold,
        ..ReadLimits::default()
    }
}

#[test]
fn store_directory_enumeration_refuses_before_collecting_unbounded_junk() {
    let dir = ScopedDir::new("directory");
    drop(Store::open(&dir, FoldCfg::default()).unwrap());
    std::fs::write(dir.join("junk-a"), []).unwrap();
    std::fs::write(dir.join("junk-b"), []).unwrap();

    let error = match Store::open_with_options(
        &dir,
        StoreOptions { read_limits: with_counts(2, 100, 100), ..StoreOptions::default() },
    ) {
        Ok(_) => panic!("two entries must not admit a store directory containing more"),
        Err(error) => error,
    };
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert!(matches!(
        error.downcast_ref::<ReadAdmissionError>(),
        Some(ReadAdmissionError::ObjectCountTooLarge { collection, actual: 3, allowed: 2 })
            if collection.contains("store directory")
    ));
}

#[test]
fn retained_manifest_listing_propagates_directory_open_failure() {
    let dir = ScopedDir::new("retained-io");
    std::fs::remove_dir_all(&*dir).unwrap();
    let error = turndb::store::retained_commits(&dir).unwrap_err();
    assert_eq!(classify(&error), ErrorClass::NotFound);
    assert!(error.to_string().contains("retained manifests"));
}

#[test]
fn manifest_recovery_reserves_its_staging_name_before_publication() {
    let dir = ScopedDir::new("manifest-recovery");
    let mut store = Store::open(&dir, FoldCfg::default()).unwrap();
    store.put("one", &[Span::Lit(b"one")], vec![]).unwrap();
    store.flush().unwrap();
    drop(store);

    let manifest = dir.join("MANIFEST");
    let mut damaged = std::fs::read(&manifest).unwrap();
    damaged[0] ^= 0xff;
    std::fs::write(&manifest, &damaged).unwrap();
    let root_entries = std::fs::read_dir(&*dir).unwrap().count() as u64;
    let limits = with_counts(root_entries, 100, 100);

    let error = turndb::store::recover_manifest_with_limits_and_control(
        &dir,
        FoldCfg::default(),
        turndb::store::RecoveryOptions::default(),
        limits,
        &turndb::control::OperationControl::default(),
    )
    .unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert_eq!(std::fs::read(&manifest).unwrap(), damaged);
    assert!(!dir.join("MANIFEST.tmp").exists());
}

#[test]
fn wal_frame_limit_is_enforced_before_writer_and_replay_growth() {
    let dir = ScopedDir::new("wal");
    let limits = with_counts(100, 2, 100);
    let mut store = Store::open_file_with_options(
        &store_file(&dir),
        StoreOptions { read_limits: limits, ..StoreOptions::default() },
    )
    .unwrap();
    store.put("one", &[Span::Lit(b"1")], vec![]).unwrap();
    store.put("two", &[Span::Lit(b"2")], vec![]).unwrap();
    let before = store.health();
    assert_eq!(before.wal_frames, 2);
    let error = store.put("three", &[Span::Lit(b"3")], vec![]).unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert_eq!(store.health(), before);
    store.sync().unwrap();
    drop(store);

    let wal = {
        let mut p = store_file(&dir).into_os_string();
        p.push("-wal");
        std::path::PathBuf::from(p)
    };
    let before_bytes = std::fs::metadata(&wal).unwrap().len();
    let error = match Store::open_file_with_options(
        &store_file(&dir),
        StoreOptions { read_limits: with_counts(100, 1, 100), ..StoreOptions::default() },
    ) {
        Ok(_) => panic!("one frame must not admit the two-frame WAL"),
        Err(error) => error,
    };
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), before_bytes);
}

#[test]
fn batch_frame_count_is_preflighted_before_fold_mutation() {
    let dir = ScopedDir::new("batch");
    let mut store = Store::open_file_with_options(
        &store_file(&dir),
        StoreOptions { read_limits: with_counts(100, 2, 100), ..StoreOptions::default() },
    )
    .unwrap();
    let mut batch = turndb::store::Batch::new();
    batch.put("a", &[Span::Piece(&[0x11; 32])], vec![]);
    batch.put("b", &[Span::Piece(&[0x22; 32])], vec![]);
    let before = store.health();
    let error = store.apply(batch).unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert_eq!(store.health(), before);
}

#[test]
fn fold_block_limit_preserves_progress_and_refuses_the_next_block_before_mutation() {
    let dir = ScopedDir::new("fold-write");
    let cfg = FoldCfg { block_target: 32, ..FoldCfg::default() };
    let limits = with_counts(100, 100, 1);
    let mut fold = Fold::open_with_limits(&dir, cfg, limits).unwrap();
    let first = fold.put(&[0x31; 32]).unwrap();
    fold.sync().unwrap();
    let before = fold.tail();
    let error = fold.put(&[0x72; 32]).unwrap_err();
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert_eq!(fold.tail(), before);
    assert_eq!(fold.read_verified(first.loc, first.hash).unwrap(), [0x31; 32]);
}

#[test]
fn sparse_persisted_block_id_is_refused_before_block_directory_resize() {
    let dir = ScopedDir::new("fold-sparse");
    let cfg = FoldCfg { block_target: 32, ..FoldCfg::default() };
    let mut fold = Fold::open(&dir, cfg).unwrap();
    fold.put(&[0x44; 32]).unwrap();
    fold.sync().unwrap();
    drop(fold);

    let path = segment::seg_path(&dir, 0);
    let mut file = std::fs::read(&path).unwrap();
    let start = segment::SEG_HDR_LEN as usize;
    let stored = u32::from_le_bytes(file[start + 6..start + 10].try_into().unwrap()) as usize;
    file[start + 12..start + 16].copy_from_slice(&50_000u32.to_le_bytes());
    let checksum = block::xsum(&file[start..start + block::BLOCK_HDR_LEN + stored]);
    let checksum_at = start + block::BLOCK_HDR_LEN + stored;
    file[checksum_at..checksum_at + block::BLOCK_XSUM_LEN].copy_from_slice(&checksum);
    std::fs::write(&path, file).unwrap();

    let error = match Fold::open_read_with_limits(&dir, cfg, &[], with_counts(100, 100, 4)) {
        Ok(_) => panic!("a sparse hostile block id must not size the directory"),
        Err(error) => error,
    };
    assert_eq!(classify(&error), ErrorClass::ResourceExhausted);
    assert!(error.to_string().contains("fold blocks"));
}

#[test]
fn writer_truncates_a_torn_wal_suffix_before_appending_new_frames() {
    let dir = ScopedDir::new("wal-tail");
    let mut store = Store::open_file(&store_file(&dir), FoldCfg::default()).unwrap();
    store.put("before", &[Span::Lit(b"before")], vec![]).unwrap();
    store.sync().unwrap();
    drop(store);

    let wal = {
        let mut p = store_file(&dir).into_os_string();
        p.push("-wal");
        std::path::PathBuf::from(p)
    };
    let mut file = std::fs::OpenOptions::new().append(true).open(&wal).unwrap();
    file.write_all(&[0x57, 9, 0, 0, 0, 0, 0, 0, 0, 200, 0, 0, 0]).unwrap();
    file.write_all(b"torn").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let mut recovered = Store::open_file(&store_file(&dir), FoldCfg::default()).unwrap();
    recovered.put("after", &[Span::Lit(b"after")], vec![]).unwrap();
    recovered.sync().unwrap();
    drop(recovered);

    let reopened = Store::open_file(&store_file(&dir), FoldCfg::default()).unwrap();
    assert!(reopened.get("before").unwrap().is_some());
    assert!(reopened.get("after").unwrap().is_some());
}

/// The migrated suites build single-file stores inside their temp directories: the parent is
/// ensured, the store is one file within it, and every cleanup keeps operating on the directory.
fn store_file(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).ok();
    dir.join("s.turndb")
}
