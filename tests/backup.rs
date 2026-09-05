//! Backup and restore protocol gates.

use std::path::PathBuf;
use turndb::control::{CancellationToken, OperationControl, OperationInterrupted};
use turndb::fold::FoldCfg;
use turndb::read_limits::ReadLimits;
use turndb::store::{Span, Store};

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-backup-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    // small blocks and segments so backups carry a multi-segment fold with sidecars
    FoldCfg { block_target: 4 * 1024, seg_max: 16 * 1024, ..Default::default() }
}

#[test]
fn a_brand_new_empty_store_backs_up_as_the_canonical_empty_birth() {
    let root = tmp("empty");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("empty.turndb");
    let artifact = root.join("empty-backup.turndb");
    let restored = root.join("empty-restored.turndb");
    let mut store = Store::open_file(&source, cfg()).unwrap();

    let backup = store.backup(&artifact).unwrap();
    assert_eq!(backup.members, 0);
    assert_eq!(backup.commit, 0);
    let reader = turndb::store::open_read_container(&artifact, cfg()).unwrap();
    assert!(reader.ids().unwrap().is_empty());

    let restore = turndb::store::restore_file(&artifact, &restored).unwrap();
    assert_eq!(restore.members, 0);
    assert_eq!(restore.commit, 0);
    Store::open_file(&restored, cfg()).unwrap().close().unwrap();
    store.close().unwrap();
    std::fs::remove_dir_all(root).ok();
}

fn rewrite_manifest_field(bytes: &[u8], before: &str, after: &str) -> Vec<u8> {
    let marker = b"\ncrc32=";
    let split = bytes.windows(marker.len()).position(|window| window == marker).unwrap();
    let payload = std::str::from_utf8(&bytes[..split]).unwrap();
    assert_eq!(payload.matches(before).count(), 1);
    let rewritten = payload.replacen(before, after, 1);
    let checksum = crc32fast::hash(rewritten.as_bytes());
    format!("{rewritten}\ncrc32={checksum:08x}").into_bytes()
}

fn assert_no_artifact_staging(root: &std::path::Path, final_name: &str, operation: &str) {
    let prefix = format!("{final_name}.{operation}-");
    let leftovers: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(&prefix))
        .collect();
    assert!(leftovers.is_empty(), "uninstalled artifact staging remains: {leftovers:?}");
}

#[test]
fn online_backup_is_an_exact_settled_cut_and_restore_is_writable() {
    let root = tmp("online-backup");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("store.turndb");
    let mut store = Store::open_file(&ct, cfg()).unwrap();
    store
        .put("before", &[Span::Piece(b"accepted before backup and not explicitly synced")], vec![])
        .unwrap();

    let artifact = root.join("snapshot.turndb");
    let backed_up = store.backup(&artifact).unwrap();
    assert!(backed_up.members >= 3);
    assert_eq!(backed_up.commit, store.manifest().commit);

    store.put("after", &[Span::Lit(b"later")], vec![]).unwrap();
    store.sync().unwrap();
    store.flush().unwrap();

    // Restoring fully verifies the staged copy of the backup cut before publishing a writable file.
    let restored_path = root.join("restored.turndb");
    let restored = turndb::store::restore_file(&artifact, &restored_path).unwrap();
    assert_eq!(restored.members, backed_up.members);
    assert_eq!(restored.commit, backed_up.commit);

    let mut reopened = Store::open_file(&restored_path, cfg()).unwrap();
    assert!(reopened.reconstruct("before").unwrap().is_some());
    assert!(
        reopened.reconstruct("after").unwrap().is_none(),
        "backup must remain the point-in-time cut it published"
    );
    reopened.put("restored-write", &[Span::Lit(b"works")], vec![]).unwrap();
    reopened.sync().unwrap();
    reopened.flush().unwrap();
    assert!(reopened.reconstruct("restored-write").unwrap().is_some());
    drop(reopened);
    // The backup is already an ordinary store; restore does not mutate the source artifact.
    let backup = Store::open_file(&artifact, cfg()).unwrap();
    assert!(backup.reconstruct("before").unwrap().is_some());
    backup.close().unwrap();
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn backup_and_restore_never_replace_destinations_or_publish_corruption() {
    let root = tmp("safe-destinations");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("store.turndb");
    let mut store = Store::open_file(&ct, cfg()).unwrap();
    store
        .put("kept", &[Span::Piece(b"bytes worth backing up, long enough to fold")], vec![])
        .unwrap();
    store.sync().unwrap();
    store.flush().unwrap();

    let artifact = root.join("snapshot.turndb");
    std::fs::write(&artifact, b"belongs to the caller").unwrap();
    let before = std::fs::read(&artifact).unwrap();
    assert!(store.backup(&artifact).is_err(), "an existing destination is never replaced");
    assert_eq!(std::fs::read(&artifact).unwrap(), before);

    std::fs::remove_file(&artifact).unwrap();
    store.backup(&artifact).unwrap();
    store.close().unwrap();

    let error = turndb::store::restore_file(&artifact, &ct).unwrap_err();
    assert!(error.to_string().contains("exists"), "restore must refuse a live path: {error:#}");

    // A committed container can never become an empty store merely by losing its manifest. The
    // remaining members are self-consistent bytes, but without authority they are corruption.
    let authorityless_path = root.join("authorityless.turndb");
    std::fs::copy(&artifact, &authorityless_path).unwrap();
    let mut authorityless = turndb::container::Container::open(&authorityless_path).unwrap();
    assert!(authorityless.remove("MANIFEST").unwrap());
    authorityless.commit().unwrap();
    drop(authorityless);
    let absent = root.join("authorityless-must-remain-absent.turndb");
    let error = turndb::store::restore_file(&authorityless_path, &absent).unwrap_err();
    assert_eq!(
        turndb::error::classify(&error),
        turndb::error::ErrorClass::Corruption,
        "a committed store without manifest authority is corruption: {error:#}"
    );
    assert!(!absent.exists());
    assert_no_artifact_staging(&root, "authorityless-must-remain-absent.turndb", "restoring");

    // A backup whose member bytes drifted must refuse restoration and leave nothing behind.
    let mut corrupt = std::fs::read(&artifact).unwrap();
    let at = turndb::container::REGION_START as usize;
    corrupt[at] ^= 1;
    let corrupt_path = root.join("corrupt.turndb");
    std::fs::write(&corrupt_path, corrupt).unwrap();
    let absent = root.join("must-remain-absent.turndb");
    let error = turndb::store::restore_file(&corrupt_path, &absent).unwrap_err();
    assert_eq!(
        turndb::error::classify(&error),
        turndb::error::ErrorClass::Corruption,
        "a failed member checksum is corruption: {error:#}"
    );
    assert!(!absent.exists());
    assert!(
        std::fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".restoring")),
        "a refused restore must remove its staging"
    );

    // Member checksums alone are insufficient: a self-consistent container whose MANIFEST names
    // members it does not carry must also refuse before publication.
    let valid = turndb::container::Container::open(&artifact).unwrap();
    let manifest = valid.read_file_bounded("MANIFEST", turndb::store::MAX_MANIFEST_BYTES).unwrap();
    drop(valid);
    let inconsistent_path = root.join("inconsistent.turndb");
    let mut inconsistent = turndb::container::Container::create(&inconsistent_path).unwrap();
    inconsistent.put_bytes("MANIFEST", &manifest).unwrap();
    inconsistent.commit().unwrap();
    drop(inconsistent);
    let absent = root.join("must-also-remain-absent.turndb");
    let error = turndb::store::restore_file(&inconsistent_path, &absent).unwrap_err();
    assert_eq!(
        turndb::error::classify(&error),
        turndb::error::ErrorClass::Corruption,
        "a manifest that names absent storage is corruption: {error:#}"
    );
    assert!(!absent.exists());

    let malformed_path = root.join("malformed.turndb");
    std::fs::write(&malformed_path, b"present, but not a container").unwrap();
    let absent = root.join("malformed-must-remain-absent.turndb");
    let error = turndb::store::restore_file(&malformed_path, &absent).unwrap_err();
    assert_eq!(
        turndb::error::classify(&error),
        turndb::error::ErrorClass::Corruption,
        "explicit artifact validation classifies malformed persisted bytes: {error:#}"
    );
    assert!(!absent.exists());
    assert_no_artifact_staging(&root, "malformed-must-remain-absent.turndb", "restoring");

    // A container checksum can be perfectly self-consistent while the bytes its manifest commits
    // end after the fold's last complete frame. Full staged-store validation must refuse that
    // semantic corruption before the destination name exists.
    let trailing_path = root.join("trailing-fold-byte.turndb");
    std::fs::copy(&artifact, &trailing_path).unwrap();
    let mut trailing = turndb::container::Container::open(&trailing_path).unwrap();
    let authority =
        trailing.read_file_bounded("MANIFEST", turndb::store::MAX_MANIFEST_BYTES).unwrap();
    let split = authority.windows(7).position(|window| window == b"\ncrc32=").unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&authority[..split]).unwrap();
    let generation = manifest["fold_gen"].as_u64().unwrap();
    let segment = manifest["fold_seg"].as_u64().unwrap();
    let old_tail = manifest["fold_off"].as_u64().unwrap();
    let prefix = if generation == 0 { "fold".to_string() } else { format!("fold-{generation:04}") };
    let member = format!("{prefix}/seg-{segment:08}.fold");
    trailing
        .append_stream(&member, 1, |_, into| {
            into.fill(0x7f);
            Ok(())
        })
        .unwrap();
    let rewritten = rewrite_manifest_field(
        &authority,
        &format!("\"fold_off\":{old_tail}"),
        &format!("\"fold_off\":{}", old_tail + 1),
    );
    trailing.put_bytes("MANIFEST", &rewritten).unwrap();
    trailing.commit().unwrap();
    drop(trailing);
    let absent = root.join("trailing-fold-must-remain-absent.turndb");
    let error = turndb::store::restore_file(&trailing_path, &absent).unwrap_err();
    assert_eq!(
        turndb::error::classify(&error),
        turndb::error::ErrorClass::Corruption,
        "a committed partial frame is corruption: {error:#}"
    );
    assert!(!absent.exists());
    assert_no_artifact_staging(&root, "trailing-fold-must-remain-absent.turndb", "restoring");

    // A replacement part can be internally valid and reconstruct perfectly while still violating
    // the live manifest's BLAKE3 authority pin. Artifacts omit retained manifests, so the live
    // pins themselves are load-bearing verification evidence.
    let other_source = root.join("other-source.turndb");
    let mut other = Store::open_file(&other_source, cfg()).unwrap();
    other.put("other", &[Span::Piece(b"different but internally valid content")], vec![]).unwrap();
    let other_artifact = root.join("other-artifact.turndb");
    other.backup(&other_artifact).unwrap();
    other.close().unwrap();

    let authority = turndb::container::Container::open(&artifact).unwrap();
    let replacement = turndb::container::Container::open(&other_artifact).unwrap();
    let hybrid_path = root.join("hybrid.turndb");
    let mut hybrid = turndb::container::Container::create(&hybrid_path).unwrap();
    for name in replacement.names() {
        let source = if name == "MANIFEST" { &authority } else { &replacement };
        let bytes = source.read_file_bounded(name, u64::MAX).unwrap();
        hybrid.put_bytes(name, &bytes).unwrap();
    }
    hybrid.commit().unwrap();
    drop(hybrid);
    let absent = root.join("hybrid-must-remain-absent.turndb");
    let error = turndb::store::restore_file(&hybrid_path, &absent).unwrap_err();
    assert_eq!(
        turndb::error::classify(&error),
        turndb::error::ErrorClass::Corruption,
        "a valid replacement part must fail the live manifest pin: {error:#}"
    );
    assert!(!absent.exists());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn artifact_staging_is_unique_and_retired_exact_suffixes_are_ordinary_names() {
    let root = tmp("staging-alias");
    std::fs::create_dir_all(&root).unwrap();

    let destination = root.join("current.turndb");
    let source = root.join("current.turndb.backing-up");
    let mut store = Store::open_file(&source, cfg()).unwrap();
    store.put("still-live", &[Span::Lit(b"not yet settled")], vec![]).unwrap();
    store.backup(&destination).unwrap();
    assert!(source.exists(), "the retired exact suffix was touched as protocol state");
    assert!(destination.exists());
    assert!(store.reconstruct("still-live").unwrap().is_some());
    store.close().unwrap();

    let near_source = root.join("near.turndb.backing-up-copy");
    let near_destination = root.join("near.turndb");
    let mut near = Store::open_file(&near_source, cfg()).unwrap();
    near.put("valid", &[Span::Lit(b"nearest valid name")], vec![]).unwrap();
    near.backup(&near_destination).unwrap();
    assert!(near_source.exists());
    assert!(near_destination.exists());
    near.close().unwrap();

    let restore_source = root.join("restore-target.turndb.restoring");
    let restore_destination = root.join("restore-target.turndb");
    let mut origin = Store::open_file(&root.join("origin.turndb"), cfg()).unwrap();
    origin.put("kept", &[Span::Lit(b"restore source")], vec![]).unwrap();
    origin.backup(&restore_source).unwrap();
    origin.close().unwrap();
    turndb::store::restore_file(&restore_source, &restore_destination).unwrap();
    assert!(restore_source.exists(), "the retired exact suffix was touched as protocol state");
    assert!(restore_destination.exists());

    for reserved in [
        "store.turndb-wal",
        "store.turndb.reclaimed",
        "store.turndb.backing-up-12-0",
        "store.turndb.restoring-12-0",
        "store.turndb.publish-12-0",
    ] {
        let path = root.join(reserved);
        let error = Store::open_file(&path, cfg()).err().expect("reserved path must refuse");
        assert_eq!(
            turndb::error::classify(&error),
            turndb::error::ErrorClass::InvalidArgument,
            "{reserved}: {error:#}"
        );
        assert!(!path.exists(), "{reserved} was created before refusal");
    }

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn concurrent_restores_use_disjoint_staging_and_install_one_whole_destination() {
    let root = tmp("concurrent-restore-staging");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("source.turndb");
    let mut store = Store::open_file(&source, cfg()).unwrap();
    store.put("record", &[Span::Piece(&vec![0x5a; 8 << 20])], vec![]).unwrap();
    let artifact = root.join("artifact.turndb");
    store.backup(&artifact).unwrap();
    store.close().unwrap();

    let destination = root.join("destination.turndb");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let artifact = artifact.clone();
        let destination = destination.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            turndb::store::restore_file(&artifact, &destination)
        }));
    }
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1, "{results:?}");
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1, "{results:?}");
    let reader = turndb::store::open_read_container(&destination, cfg()).unwrap();
    assert_eq!(reader.reconstruct("record").unwrap().unwrap(), vec![0x5a; 8 << 20]);
    assert_no_artifact_staging(&root, "destination.turndb", "restoring");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn restore_applies_the_callers_read_admission_to_staging() {
    let root = tmp("restore-read-limits");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("source.turndb");
    let mut store = Store::open_file(&source, cfg()).unwrap();
    store.put("kept", &[Span::Piece(b"content for the admitted backup")], vec![]).unwrap();
    let artifact = root.join("artifact.turndb");
    store.backup(&artifact).unwrap();
    store.close().unwrap();

    let refused = root.join("refused.turndb");
    let error = turndb::store::restore_file_with_limits(
        &artifact,
        &refused,
        ReadLimits { max_directory_entries: 1, ..ReadLimits::default() },
    )
    .unwrap_err();
    assert_eq!(turndb::error::classify(&error), turndb::error::ErrorClass::ResourceExhausted);
    assert!(!refused.exists());
    assert_no_artifact_staging(&root, "refused.turndb", "restoring");

    let admitted = root.join("admitted.turndb");
    turndb::store::restore_file_with_limits(&artifact, &admitted, ReadLimits::default()).unwrap();
    assert!(admitted.exists());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn cancellation_never_publishes_backup_or_restore_staging() {
    let root = tmp("cancel");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("store.turndb");
    let mut store = Store::open_file(&ct, cfg()).unwrap();
    store
        .put("kept", &[Span::Piece(b"a body to carry across, long enough to fold")], vec![])
        .unwrap();
    store.sync().unwrap();
    store.flush().unwrap();

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let control = OperationControl { deadline: None, cancellation: Some(cancellation.clone()) };
    let artifact = root.join("cancelled.turndb");
    let error = store.backup_with_control(&artifact, &control).unwrap_err();
    assert!(error.downcast_ref::<OperationInterrupted>().is_some());
    assert!(!artifact.exists());

    let source = root.join("source.turndb");
    store.backup(&source).unwrap();
    store.close().unwrap();

    let destination = root.join("cancelled-restore.turndb");
    let error =
        turndb::store::restore_file_with_control(&source, &destination, &control).unwrap_err();
    assert!(error.downcast_ref::<OperationInterrupted>().is_some());
    assert!(!destination.exists());
    assert!(
        std::fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".restoring")),
        "a cancelled restore must remove its staging"
    );
    std::fs::remove_dir_all(root).ok();
}
