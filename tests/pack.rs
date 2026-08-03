//! The pack gate: a store in one file answers exactly as the directory did, and both crossings
//! are mechanical.

use std::path::{Path, PathBuf};
use turndb::control::{CancellationToken, OperationControl, OperationInterrupted};
use turndb::fold::FoldCfg;
use turndb::store::{Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-pack-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    // small blocks and segments so the pack carries a multi-segment fold with sidecars
    FoldCfg { block_target: 4 * 1024, seg_max: 16 * 1024, ..Default::default() }
}

/// Deterministic incompressible bytes — segments must actually roll.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut h = blake3::hash(&seed.to_le_bytes());
    while out.len() < len {
        out.extend_from_slice(h.as_bytes());
        h = blake3::hash(h.as_bytes());
    }
    out.truncate(len);
    out
}

/// A store with several flush intervals, a merge, a delete, and enough content to roll segments.
fn build_store(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut s = Store::open(dir, cfg()).unwrap();
    let mut want = Vec::new();
    for round in 0..3 {
        for i in 0..12 {
            let id = format!("r{round}:{i:02}");
            let body = noise(round as u64 * 100 + i as u64, 1800);
            s.put(
                &id,
                &[Span::Lit(b"["), Span::Piece(&body), Span::Lit(b"]")],
                vec![
                    ("model".into(), AttrValue::Str(format!("m{}", i % 2))),
                    ("n".into(), AttrValue::Int(i)),
                ],
            )
            .unwrap();
            let mut w = b"[".to_vec();
            w.extend_from_slice(&body);
            w.extend_from_slice(b"]");
            want.push((id, w));
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    s.delete("r0:00").unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    want.retain(|(id, _)| id != "r0:00");
    s.merge_range(0, 2).unwrap().unwrap();
    assert!(s.fold().segment_count() > 1, "the fixture must roll at least one segment");
    want
}

#[test]
fn a_pack_answers_identically_to_the_directory_it_came_from() {
    let root = tmp("roundtrip");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    let want = build_store(&dir);

    let pk = root.join("snapshot.turndb");
    let stats = turndb::pack::write(&dir, &pk).unwrap();
    assert!(stats.files > 3, "manifest + parts + segments: {stats:?}");
    assert_eq!(turndb::pack::Pack::open(&pk).unwrap().verify().unwrap(), stats.files);

    let from_dir = Store::open_read(&dir, cfg()).unwrap();
    let from_pack = turndb::store::open_read_pack(&pk, cfg()).unwrap();
    assert_eq!(from_dir.ids().unwrap(), from_pack.ids().unwrap());
    for (id, body) in &want {
        assert_eq!(
            from_pack.reconstruct(id).unwrap().unwrap(),
            *body,
            "{id} must reconstruct identically out of the pack"
        );
        let a = from_dir.get(id).unwrap().unwrap();
        let b = from_pack.get(id).unwrap().unwrap();
        assert_eq!(a, b, "{id} record must match field for field");
    }
    assert!(from_pack.reconstruct("r0:00").unwrap().is_none(), "the delete holds inside the pack");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn unpacking_yields_an_ordinary_writable_store() {
    let root = tmp("unpack");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    let want = build_store(&dir);

    let pk = root.join("snapshot.turndb");
    turndb::pack::write(&dir, &pk).unwrap();
    let out = root.join("restored");
    turndb::pack::unpack(&pk, &out).unwrap();

    // The restored directory is a full store: readable, and the WRITER ROLE is available again.
    let mut s = Store::open(&out, cfg()).unwrap();
    for (id, body) in &want {
        assert_eq!(s.reconstruct(id).unwrap().unwrap(), *body, "{id} lost in the crossing");
    }
    s.put("after:restore", &[Span::Piece(b"written after unpacking, long enough to fold")], vec![])
        .unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    assert!(s.reconstruct("after:restore").unwrap().is_some());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_pack_refuses_what_it_must() {
    let root = tmp("refuse");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    build_store(&dir);

    // A directory packer must not race an already-open writer.
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        s.put("unflushed", &[Span::Lit(b"x")], vec![]).unwrap();
        s.sync().unwrap();
        assert!(
            turndb::pack::write(&dir, &root.join("no.turndb")).is_err(),
            "a live writer must refuse a second writer role"
        );
        s.flush().unwrap();
    }
    let pk = root.join("snapshot.turndb");
    turndb::pack::write(&dir, &pk).unwrap();
    let pristine = std::fs::read(&pk).unwrap();

    // torn footer
    std::fs::write(&pk, &pristine[..pristine.len() - 7]).unwrap();
    assert!(turndb::pack::Pack::open(&pk).is_err(), "a torn footer must refuse");

    // future version
    let mut b = pristine.clone();
    let vat = b.len() - 40 + 29;
    b[vat] = 9;
    // re-seal the footer checksum so ONLY the version differs
    let foot_start = b.len() - 40;
    let x = blake3::hash(&b[foot_start..foot_start + 36]);
    let xat = b.len() - 4;
    b[xat..].copy_from_slice(&x.as_bytes()[0..4]);
    std::fs::write(&pk, &b).unwrap();
    let err = match turndb::pack::Pack::open(&pk) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("must refuse"),
    };
    assert!(err.contains("version"), "a future version must refuse by name, got: {err}");

    // reserved bytes are enforced, not decorative
    let mut b = pristine.clone();
    let rat = b.len() - 40 + 30;
    b[rat] = 1;
    let x = blake3::hash(&b[foot_start..foot_start + 36]);
    b[xat..].copy_from_slice(&x.as_bytes()[0..4]);
    std::fs::write(&pk, &b).unwrap();
    let err = match turndb::pack::Pack::open(&pk) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("must refuse"),
    };
    assert!(err.contains("reserved"), "non-zero reserved bytes must refuse, got: {err}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn directory_backup_recovers_and_includes_a_durable_wal() {
    let root = tmp("durable-wal");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    {
        let mut store = Store::open(&dir, cfg()).unwrap();
        store
            .put("wal-record", &[Span::Piece(b"durable but not explicitly flushed")], vec![])
            .unwrap();
        store.sync().unwrap();
    }

    let artifact = root.join("snapshot.turndb");
    turndb::pack::write(&dir, &artifact).unwrap();
    let snapshot = turndb::store::open_read_pack(&artifact, cfg()).unwrap();
    assert_eq!(
        snapshot.reconstruct("wal-record").unwrap().unwrap(),
        b"durable but not explicitly flushed"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn online_backup_is_an_exact_settled_cut_and_restore_is_writable() {
    let root = tmp("online-backup");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    let mut store = Store::open(&dir, cfg()).unwrap();
    store
        .put("before", &[Span::Piece(b"accepted before backup and not explicitly synced")], vec![])
        .unwrap();

    let artifact = root.join("snapshot.turndb");
    let backed_up = store.backup(&artifact).unwrap();
    assert!(backed_up.files >= 3);
    assert_eq!(backed_up.bytes, std::fs::metadata(&artifact).unwrap().len());
    assert_eq!(backed_up.commit, store.manifest().commit);

    store.put("after", &[Span::Lit(b"later")], vec![]).unwrap();
    store.sync().unwrap();
    store.flush().unwrap();

    let restored_dir = root.join("restored");
    let restored = turndb::pack::restore(&artifact, &restored_dir).unwrap();
    assert_eq!(restored.files, backed_up.files);
    assert_eq!(restored.bytes, backed_up.bytes);
    assert_eq!(restored.commit, backed_up.commit);

    let mut reopened = Store::open(&restored_dir, cfg()).unwrap();
    assert!(reopened.reconstruct("before").unwrap().is_some());
    assert!(
        reopened.reconstruct("after").unwrap().is_none(),
        "backup must remain an immutable cut"
    );
    reopened.put("restored-write", &[Span::Lit(b"works")], vec![]).unwrap();
    reopened.sync().unwrap();
    reopened.flush().unwrap();
    assert!(reopened.reconstruct("restored-write").unwrap().is_some());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn backup_and_restore_never_replace_destinations_or_publish_corruption() {
    let root = tmp("safe-destinations");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    build_store(&dir);

    let artifact = root.join("snapshot.turndb");
    std::fs::write(&artifact, b"belongs to the caller").unwrap();
    let before = std::fs::read(&artifact).unwrap();
    let error = turndb::pack::write(&dir, &artifact).unwrap_err();
    assert!(error.downcast_ref::<turndb::pack::BackupError>().is_some());
    assert_eq!(std::fs::read(&artifact).unwrap(), before);

    std::fs::remove_file(&artifact).unwrap();
    turndb::pack::write(&dir, &artifact).unwrap();
    let existing = root.join("existing");
    std::fs::create_dir(&existing).unwrap();
    let marker = existing.join("keep");
    std::fs::write(&marker, b"untouched").unwrap();
    let error = turndb::pack::restore(&artifact, &existing).unwrap_err();
    assert!(error.downcast_ref::<turndb::pack::BackupError>().is_some());
    assert_eq!(std::fs::read(&marker).unwrap(), b"untouched");

    let mut corrupt = std::fs::read(&artifact).unwrap();
    corrupt[0] ^= 1;
    let corrupt_path = root.join("corrupt.turndb");
    std::fs::write(&corrupt_path, corrupt).unwrap();
    let absent = root.join("must-remain-absent");
    let error = turndb::pack::restore(&corrupt_path, &absent).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<turndb::pack::BackupError>(),
        Some(turndb::pack::BackupError::InvalidBackup { .. })
    ));
    assert!(!absent.exists());
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("turndb-restore")));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn cancellation_never_publishes_backup_or_restore_staging() {
    let root = tmp("cancel");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    build_store(&dir);

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let control = OperationControl { deadline: None, cancellation: Some(cancellation.clone()) };
    let artifact = root.join("cancelled.turndb");
    let error = turndb::pack::write_with_control(&dir, &artifact, &control).unwrap_err();
    assert!(error.downcast_ref::<OperationInterrupted>().is_some());
    assert!(!artifact.exists());
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("turndb-pack")));

    let source = root.join("source.turndb");
    turndb::pack::write(&dir, &source).unwrap();
    let error = match turndb::pack::Pack::open_with_control(&source, &control) {
        Ok(_) => panic!("a cancelled pack open must refuse"),
        Err(error) => error,
    };
    assert!(error.downcast_ref::<OperationInterrupted>().is_some());
    let pack = turndb::pack::Pack::open(&source).unwrap();
    let error = pack.verify_with_control(&control).unwrap_err();
    assert!(error.downcast_ref::<OperationInterrupted>().is_some());

    let destination = root.join("cancelled-restore");
    let error = turndb::pack::restore_with_control(&source, &destination, &control).unwrap_err();
    assert!(error.downcast_ref::<OperationInterrupted>().is_some());
    assert!(!destination.exists());
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("turndb-restore")));
    std::fs::remove_dir_all(root).ok();
}
