//! The container gate: a store in one MUTABLE file answers exactly as the directory did, grows
//! without invalidating what a reader already resolved, and survives a torn commit.

use std::path::{Path, PathBuf};
use turndb::container::{Container, CONTAINER_VERSION, MAGIC, SLOT_LEN};
use turndb::fold::FoldCfg;
use turndb::store::{checkpoint_into_container, open_read_container, Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-container-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    // small blocks and segments so the fixture carries a multi-segment fold with sidecars
    FoldCfg { block_target: 4 * 1024, seg_max: 16 * 1024, ..Default::default() }
}

fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Byte offset of the slot holding the highest commit sequence.
fn newest_slot(bytes: &[u8]) -> usize {
    let slot1 = SLOT_LEN as usize;
    let seq_of = |at: usize| {
        if &bytes[at..at + 8] == MAGIC {
            u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap())
        } else {
            0
        }
    };
    if seq_of(slot1) > seq_of(0) {
        slot1
    } else {
        0
    }
}

/// A store with several parts, a rolled fold, and a delete — the shapes a checkpoint must carry.
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
    assert!(s.fold().segment_count() > 1, "the fixture must roll at least one segment");
    want
}

#[test]
fn a_container_answers_identically_to_the_directory_it_came_from() {
    let root = tmp("roundtrip");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    let want = build_store(&dir);

    let ct = root.join("store.turndb");
    let stats = checkpoint_into_container(&dir, &ct).unwrap();
    assert!(stats.members > 3, "manifest + parts + segments: {stats:?}");
    assert_eq!(stats.commit_seq, 1, "the first checkpoint is commit 1");
    assert_eq!(Container::open(&ct).unwrap().verify().unwrap(), stats.members);

    let from_dir = Store::open_read(&dir, cfg()).unwrap();
    let from_container = open_read_container(&ct, cfg()).unwrap();
    assert_eq!(from_dir.ids().unwrap(), from_container.ids().unwrap());
    for (id, body) in &want {
        assert_eq!(
            from_container.reconstruct(id).unwrap().unwrap(),
            *body,
            "{id} must reconstruct byte-exact out of the container"
        );
        assert_eq!(
            from_dir.get(id).unwrap().unwrap(),
            from_container.get(id).unwrap().unwrap(),
            "{id} record must match field for field"
        );
    }
    assert!(
        from_container.reconstruct("r0:00").unwrap().is_none(),
        "the delete holds inside the container"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_second_checkpoint_reingests_only_what_changed() {
    let root = tmp("incremental");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    build_store(&dir);

    let ct = root.join("store.turndb");
    let first = checkpoint_into_container(&dir, &ct).unwrap();
    assert_eq!(first.skipped_members, 0, "nothing exists to skip on the first pass");

    // No writes between the two: every immutable member is already present at the same length, so
    // only MANIFEST and the advisory sidecars are restaged.
    let second = checkpoint_into_container(&dir, &ct).unwrap();
    assert_eq!(second.commit_seq, 2, "each checkpoint publishes one new state");
    assert!(
        second.skipped_members > 0 && second.skipped_members < second.members,
        "immutable members skip, mutable ones restage: {second:?}"
    );
    assert!(
        second.ingested_bytes < first.ingested_bytes,
        "an incremental checkpoint writes strictly less: {second:?} vs {first:?}"
    );
    assert!(second.free_bytes > 0, "restaged members supersede their old extents");

    // And the container still answers.
    let read = open_read_container(&ct, cfg()).unwrap();
    assert!(!read.ids().unwrap().is_empty());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn superseded_space_is_still_reported_after_a_reopen() {
    let root = tmp("freelist");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("free.turndb");

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("rewritten", &noise(1, 5000)).unwrap();
    c.commit().unwrap();
    c.put_bytes("rewritten", &noise(2, 5000)).unwrap();
    c.commit().unwrap();
    let staged = c.free_bytes();
    assert_eq!(staged, 5000, "the superseded extent is waste the container still carries");
    drop(c);

    // The free list has to round-trip through the directory, or a container reports itself compact
    // however much waste it is carrying and nothing ever schedules a rewrite.
    let reopened = Container::open(&ct).unwrap();
    assert_eq!(reopened.free_bytes(), staged, "superseded bytes must survive a reopen");
    assert_eq!(reopened.member_bytes(), 5000, "only the live extent counts as a member");
    assert_eq!(reopened.verify().unwrap(), 1);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn growth_does_not_disturb_the_state_a_reader_already_resolved() {
    let root = tmp("grow");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("grow.turndb");

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("alpha", b"first").unwrap();
    let seq1 = c.commit().unwrap();
    drop(c);

    // A reader pinned to commit 1.
    let reader = Container::open(&ct).unwrap();
    assert_eq!(reader.seq(), seq1);

    // A writer appends and commits past it.
    let mut w = Container::open(&ct).unwrap();
    w.put_bytes("beta", &noise(7, 40_000)).unwrap();
    let seq2 = w.commit().unwrap();
    assert_eq!(seq2, seq1 + 1);
    drop(w);

    // The old handle still reads exactly what it resolved: appends land beyond its tail and its
    // superblock slot was never the one written.
    assert_eq!(reader.read_file_bounded("alpha", 64).unwrap(), b"first");
    assert!(!reader.contains("beta"), "a pinned reader must not see a later commit");

    let fresh = Container::open(&ct).unwrap();
    assert_eq!(fresh.seq(), seq2);
    assert!(fresh.contains("beta"));
    assert_eq!(fresh.verify().unwrap(), 2);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_torn_commit_falls_back_to_the_previous_state() {
    let root = tmp("torn");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("torn.turndb");

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("keep", b"durable").unwrap();
    c.commit().unwrap();
    c.put_bytes("also", b"second").unwrap();
    let good = c.commit().unwrap();
    assert_eq!(good, 2);
    drop(c);

    // Corrupt whichever slot holds the newest commit, the way a torn write would: the magic
    // survives, the checksum does not. Slots alternate, so which one that is follows the count.
    let mut bytes = std::fs::read(&ct).unwrap();
    let newest = newest_slot(&bytes);
    bytes[newest + 16] ^= 0xff;
    std::fs::write(&ct, &bytes).unwrap();

    // The older slot is intact, so the container opens at commit 1 rather than refusing.
    let recovered = Container::open(&ct).unwrap();
    assert_eq!(recovered.seq(), 1, "a torn newest slot loses to the previous one");
    assert_eq!(recovered.read_file_bounded("keep", 64).unwrap(), b"durable");
    assert!(!recovered.contains("also"), "the torn commit's members are not visible");
    assert_eq!(recovered.verify().unwrap(), 1);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn both_superblocks_unreadable_is_a_refusal_not_an_empty_container() {
    let root = tmp("blind");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("blind.turndb");

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("payload", b"bytes that must not be declared absent").unwrap();
    c.commit().unwrap();
    drop(c);

    let mut bytes = std::fs::read(&ct).unwrap();
    for slot in [0usize, SLOT_LEN as usize] {
        bytes[slot + 52] ^= 0xff;
    }
    std::fs::write(&ct, &bytes).unwrap();

    let err = match Container::open(&ct) {
        Ok(_) => panic!("an unreadable head must refuse"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("superblock") || err.contains("not a container"),
        "an unreadable head must refuse loudly, got: {err}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_container_refuses_what_it_must() {
    let root = tmp("refuse");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("refuse.turndb");

    let mut c = Container::create(&ct).unwrap();
    for bad in ["", "../escape", "/absolute", "back\\slash", "nested/../up"] {
        assert!(c.put_bytes(bad, b"x").is_err(), "{bad:?} must be refused as a member name");
    }
    c.put_bytes("fine/nested/name", b"ok").unwrap();
    c.commit().unwrap();
    drop(c);

    // Creating over an existing path is refused; publication is the caller's to sequence.
    assert!(Container::create(&ct).is_err(), "create must refuse an existing path");
    // Opening something that is not a container refuses rather than inventing an empty one.
    let plain = root.join("plain.bin");
    std::fs::write(&plain, vec![0u8; 16384]).unwrap();
    assert!(Container::open(&plain).is_err(), "a non-container file must refuse");
    let absent = root.join("nope.turndb");
    assert!(Container::open(&absent).is_err(), "an absent path must refuse");
    assert!(!absent.exists(), "a refused open must not leave a file behind");

    // A member whose bytes drifted fails verification even though the directory still parses.
    let mut bytes = std::fs::read(&ct).unwrap();
    let at = turndb::container::REGION_START as usize;
    bytes[at] ^= 0xff;
    std::fs::write(&ct, &bytes).unwrap();
    let c = Container::open(&ct).unwrap();
    assert!(c.verify().is_err(), "a mutated member must fail its checksum");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_container_plane_versions_independently() {
    let root = tmp("version");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("version.turndb");

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("member", b"payload").unwrap();
    c.commit().unwrap();
    drop(c);

    // A superblock from a future container revision must be refused, not misparsed — and because
    // the version byte is inside the checksummed prefix, forging one requires re-checksumming,
    // which is exactly what a future writer would do.
    let mut bytes = std::fs::read(&ct).unwrap();
    let live = newest_slot(&bytes);
    bytes[live + 49] = CONTAINER_VERSION + 1;
    let digest = blake3::hash(&bytes[live..live + 52]);
    bytes[live + 52..live + 56].copy_from_slice(&digest.as_bytes()[0..4]);
    std::fs::write(&ct, &bytes).unwrap();

    // The older slot is still perfectly readable, and that is exactly why this must refuse: a
    // checksum-valid superblock from a newer writer is an authentic claim, so falling back to the
    // previous commit would serve a stale state while reporting success.
    let err = match Container::open(&ct) {
        Ok(_) => {
            panic!("a container from a newer revision must reject forward rather than misread")
        }
        Err(e) => e.to_string(),
    };
    assert!(err.contains("version"), "the refusal must name the version lever, got: {err}");
    std::fs::remove_dir_all(&root).ok();
}
