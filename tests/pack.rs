//! The pack gate, for what a pack still is: a retired, immutable artifact the reader and the
//! converter must keep taking — plus the native backup/restore crossing that replaced it.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use turndb::control::{CancellationToken, OperationControl, OperationInterrupted};
use turndb::fold::FoldCfg;
use turndb::store::{Span, Store};

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-pack-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    // small blocks and segments so backups carry a multi-segment fold with sidecars
    FoldCfg { block_target: 4 * 1024, seg_max: 16 * 1024, ..Default::default() }
}

/// The checked-in version-one pack: the artifact a consumer actually shipped, and therefore the
/// bytes every surviving pack surface is proven against — nothing in this codebase can write a
/// pack any more, which is the point.
fn fixture_pack_bytes() -> Vec<u8> {
    let hex_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("bindings/node/qualification/fixtures/revision-one.turndb.hex");
    let hex = std::fs::read_to_string(&hex_path).unwrap();
    let digits: Vec<u8> = hex.bytes().filter(u8::is_ascii_hexdigit).collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn write_sparse_pack_footer(path: &Path, toc_stored: u32, toc_raw: u32, files: u32) {
    // A minimal hostile artifact: nothing but a footer whose metadata claims whatever the test
    // needs, checksummed so only admission (not integrity) is what refuses it.
    let mut footer = Vec::with_capacity(40);
    footer.extend_from_slice(b"TURNPACK");
    footer.extend_from_slice(&0u64.to_le_bytes());
    footer.extend_from_slice(&toc_stored.to_le_bytes());
    footer.extend_from_slice(&toc_raw.to_le_bytes());
    footer.extend_from_slice(&files.to_le_bytes());
    footer.push(1); // toc codec
    footer.push(1); // version
    footer.extend_from_slice(&[0u8; 6]); // reserved
    let x = blake3::hash(&footer);
    footer.extend_from_slice(&x.as_bytes()[0..4]);
    assert_eq!(footer.len(), 40);
    let mut f = std::fs::File::create(path).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&footer).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn the_checked_in_pack_still_answers_byte_exact() {
    let root = tmp("fixture-read");
    std::fs::create_dir_all(&root).unwrap();
    let pk = root.join("revision-one.turndb");
    std::fs::write(&pk, fixture_pack_bytes()).unwrap();

    let pack = turndb::pack::Pack::open(&pk).unwrap();
    assert!(pack.verify().unwrap() > 2, "manifest + parts + fold");
    drop(pack);

    let rs = turndb::store::open_read_pack(&pk, cfg()).unwrap();
    assert_eq!(rs.ids().unwrap(), vec!["legacy/0001".to_string(), "legacy/0002".to_string()]);
    assert_eq!(rs.reconstruct("legacy/0001").unwrap().unwrap(), b"revision one request");
    assert_eq!(rs.reconstruct("legacy/0002").unwrap().unwrap(), b"revision one response");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_pack_refuses_what_it_must() {
    let root = tmp("refuse");
    std::fs::create_dir_all(&root).unwrap();
    let pk = root.join("mutant.turndb");
    let pristine = fixture_pack_bytes();

    // torn footer
    std::fs::write(&pk, &pristine[..pristine.len() - 7]).unwrap();
    assert!(turndb::pack::Pack::open(&pk).is_err(), "a torn footer must refuse");

    let foot_start = pristine.len() - 40;
    let xat = pristine.len() - 4;

    // future version
    let mut b = pristine.clone();
    let vat = foot_start + 29;
    b[vat] = 9;
    // re-seal the footer checksum so ONLY the version differs
    let x = blake3::hash(&b[foot_start..foot_start + 36]);
    b[xat..].copy_from_slice(&x.as_bytes()[0..4]);
    std::fs::write(&pk, &b).unwrap();
    let err = match turndb::pack::Pack::open(&pk) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("must refuse"),
    };
    assert!(err.contains("version"), "a future version must refuse by name, got: {err}");

    // reserved bytes are enforced, not decorative
    let mut b = pristine.clone();
    let rat = foot_start + 30;
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
fn pack_metadata_admission_precedes_hostile_sparse_allocations() {
    let root = tmp("metadata-limits");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("hostile.turndb");

    write_sparse_pack_footer(&path, (turndb::pack::DEFAULT_MAX_TOC_BYTES + 1) as u32, 1, 1);
    let error =
        turndb::pack::Pack::open(&path).err().expect("stored TOC limit must refuse").to_string();
    assert!(error.contains("TOC stores") && error.contains("limit"), "{error}");

    write_sparse_pack_footer(&path, 1, (turndb::pack::DEFAULT_MAX_TOC_BYTES + 1) as u32, 1);
    let error =
        turndb::pack::Pack::open(&path).err().expect("raw TOC limit must refuse").to_string();
    assert!(error.contains("TOC expands") && error.contains("limit"), "{error}");

    write_sparse_pack_footer(&path, 1, 1, (turndb::pack::DEFAULT_MAX_PACK_FILES + 1) as u32);
    let error =
        turndb::pack::Pack::open(&path).err().expect("file-count limit must refuse").to_string();
    assert!(error.contains("files") && error.contains("limit"), "{error}");

    // Limits are embedding policy, not a format dialect: a caller can choose a stricter profile.
    let valid = root.join("valid.turndb");
    std::fs::write(&valid, fixture_pack_bytes()).unwrap();
    let strict = turndb::pack::PackLimits { max_files: 1, ..turndb::pack::PackLimits::default() };
    assert!(turndb::pack::Pack::open_with_limits(&valid, strict).is_err());
    let pack = turndb::pack::Pack::open(&valid).unwrap();
    assert!(pack.read_file_bounded("MANIFEST", 0).is_err());
    assert!(!pack
        .read_file_bounded("MANIFEST", turndb::store::MAX_MANIFEST_BYTES)
        .unwrap()
        .is_empty());
    std::fs::remove_dir_all(root).ok();
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
    let legacy_staging = root.join("snapshot.turndb.sealing");
    std::fs::write(&legacy_staging, b"interrupted pre-upgrade backup").unwrap();
    let backed_up = store.backup(&artifact).unwrap();
    assert!(!legacy_staging.exists(), "a retry removes recognized legacy backup staging");
    assert!(backed_up.files >= 3);
    assert_eq!(backed_up.commit, store.manifest().commit);

    store.put("after", &[Span::Lit(b"later")], vec![]).unwrap();
    store.sync().unwrap();
    store.flush().unwrap();

    // Restoring fully verifies the staged copy of the backup cut before publishing a writable file.
    let restored_path = root.join("restored.turndb");
    let restored = turndb::store::restore_file(&artifact, &restored_path).unwrap();
    assert_eq!(restored.files, backed_up.files);
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
    assert!(!root.join("authorityless-must-remain-absent.turndb.restoring").exists());

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
    assert!(!root.join("malformed-must-remain-absent.turndb.restoring").exists());

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
fn artifact_staging_names_can_never_alias_and_delete_the_source() {
    let root = tmp("staging-alias");
    std::fs::create_dir_all(&root).unwrap();

    for (stem, suffix) in [("current", ".backing-up"), ("legacy", ".sealing")] {
        let destination = root.join(format!("{stem}.turndb"));
        let source = root.join(format!("{stem}.turndb{suffix}"));
        let mut store = Store::open_file(&source, cfg()).unwrap();
        store.put("still-live", &[Span::Lit(b"not yet settled")], vec![]).unwrap();
        let error = store.backup(&destination).unwrap_err();
        assert_eq!(turndb::error::classify(&error), turndb::error::ErrorClass::InvalidArgument);
        assert!(source.exists(), "{suffix} collision removed the source pathname");
        assert!(!destination.exists());
        assert!(store.reconstruct("still-live").unwrap().is_some());
        store.close().unwrap();
    }

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
    let error = turndb::store::restore_file(&restore_source, &restore_destination).unwrap_err();
    assert_eq!(turndb::error::classify(&error), turndb::error::ErrorClass::InvalidArgument);
    assert!(restore_source.exists(), "restore collision removed the source pathname");
    assert!(!restore_destination.exists());

    std::fs::remove_dir_all(&root).ok();
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

    // The cancelled control refuses the pack reader's doors too.
    let pk = root.join("revision-one.turndb");
    std::fs::write(&pk, fixture_pack_bytes()).unwrap();
    let error = match turndb::pack::Pack::open_with_control(&pk, &control) {
        Ok(_) => panic!("a cancelled pack open must refuse"),
        Err(error) => error,
    };
    assert!(error.downcast_ref::<OperationInterrupted>().is_some());
    let pack = turndb::pack::Pack::open(&pk).unwrap();
    let error = pack.verify_with_control(&control).unwrap_err();
    assert!(error.downcast_ref::<OperationInterrupted>().is_some());

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
