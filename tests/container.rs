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

#[test]
fn a_container_can_be_written_to_and_stays_one_file() {
    use turndb::store::ContainerStore;

    let root = tmp("writer");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("live.turndb");

    // Nothing exists yet: opening for writing creates the container and its working directory.
    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    let hot = cs.hot_directory().to_path_buf();
    let mut want = Vec::new();
    for i in 0..8 {
        let id = format!("w:{i:02}");
        let body = noise(i, 1500);
        cs.store().put(&id, &[Span::Piece(&body)], vec![]).unwrap();
        want.push((id, body));
    }
    let stats = cs.close().unwrap();
    assert!(stats.members >= 3, "the container must hold a real store: {stats:?}");

    // The promise of the single-file shape: after a clean close, the file is the only artifact.
    assert!(ct.is_file(), "the container must be a file");
    assert!(!hot.exists(), "a clean close removes the working directory");

    // And it reads as a store, byte-exact, with no directory anywhere.
    let rs = open_read_container(&ct, cfg()).unwrap();
    for (id, body) in &want {
        assert_eq!(rs.reconstruct(id).unwrap().unwrap(), *body, "{id} must survive the round trip");
    }
    drop(rs);

    // Reopening materializes, appends, and folds back in.
    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    let extra = noise(99, 1500);
    cs.store().put("w:99", &[Span::Piece(&extra)], vec![]).unwrap();
    cs.close().unwrap();

    let rs = open_read_container(&ct, cfg()).unwrap();
    assert_eq!(
        rs.reconstruct("w:99").unwrap().unwrap(),
        extra,
        "the appended record must be there"
    );
    assert_eq!(rs.ids().unwrap().len(), want.len() + 1, "and nothing earlier may be lost");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_session_that_writes_nothing_still_leaves_a_container_that_opens() {
    use turndb::store::ContainerStore;

    let root = tmp("empty-writer");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("empty.turndb");

    // Applying no records is not an error. `turndb write new.turndb input.jsonl` reaches exactly
    // this state whenever every line of the input is skipped — a mistyped schema, an empty file —
    // and it used to leave an 8 KiB container holding no members at all, because a store that
    // never commits never writes a MANIFEST and the checkpoint ingested that name unconditionally.
    let cs = ContainerStore::open(&ct, cfg()).unwrap();
    let hot = cs.hot_directory().to_path_buf();
    cs.close().unwrap();

    assert!(ct.is_file(), "the container must exist");
    assert!(!hot.exists(), "a clean close removes the working directory");

    let rs = open_read_container(&ct, cfg()).unwrap();
    assert!(rs.ids().unwrap().is_empty(), "an empty store holds no ids");
    drop(rs);

    // And it must take writes afterwards rather than stay poisoned by its own first session.
    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    let body = noise(7, 900);
    cs.store().put("later", &[Span::Piece(&body)], vec![]).unwrap();
    cs.close().unwrap();

    let rs = open_read_container(&ct, cfg()).unwrap();
    assert_eq!(rs.reconstruct("later").unwrap().unwrap(), body, "the later write must survive");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn reopening_leaves_the_sealed_parts_in_the_container() {
    use turndb::store::ContainerStore;

    let root = tmp("no-copy");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("big.turndb");

    // Enough records, flushed repeatedly, to put several sealed parts in the container.
    let mut want = Vec::new();
    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    for round in 0..4u64 {
        for i in 0..6u64 {
            let id = format!("r:{round}:{i:02}");
            let body = noise(round * 100 + i, 4000);
            cs.store().put(&id, &[Span::Piece(&body)], vec![]).unwrap();
            want.push((id, body));
        }
        cs.store().flush().unwrap();
    }
    cs.close().unwrap();

    let sealed: Vec<String> = {
        let c = Container::open(&ct).unwrap();
        c.names().filter(|n| n.starts_with("part-")).map(String::from).collect()
    };
    assert!(sealed.len() >= 2, "the container must hold several sealed parts: {sealed:?}");

    // Reopening for writing must not copy them out. This is the whole point: the cost of opening
    // a store to append one record was the size of its entire history.
    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    let hot = cs.hot_directory().to_path_buf();
    let copied: Vec<String> = std::fs::read_dir(&hot)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("part-"))
        .collect();
    assert!(
        copied.is_empty(),
        "no sealed part may be copied into the working directory: {copied:?}"
    );

    // And the writer must still read every one of them, through the container, while open.
    for (id, body) in &want {
        assert_eq!(
            cs.store().reconstruct(id).unwrap().unwrap(),
            *body,
            "{id} must be readable from the container extent"
        );
    }

    // A further write folds back in without losing the members it never copied.
    let extra = noise(9999, 4000);
    cs.store().put("r:later", &[Span::Piece(&extra)], vec![]).unwrap();
    cs.close().unwrap();

    let rs = open_read_container(&ct, cfg()).unwrap();
    for (id, body) in &want {
        assert_eq!(rs.reconstruct(id).unwrap().unwrap(), *body, "{id} must survive the reopen");
    }
    assert_eq!(rs.reconstruct("r:later").unwrap().unwrap(), extra);
    assert!(!hot.exists(), "a clean close removes the working directory");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_abandoned_working_directory_is_resumed_rather_than_discarded() {
    use turndb::store::ContainerStore;

    let root = tmp("resume");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("crash.turndb");

    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    let first = noise(1, 900);
    cs.store().put("kept:1", &[Span::Piece(&first)], vec![]).unwrap();
    cs.close().unwrap();

    // A session that acknowledges a write and then dies: the hot directory outlives it holding
    // state the container was never told about.
    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    let hot = cs.hot_directory().to_path_buf();
    let second = noise(2, 900);
    cs.store().put("kept:2", &[Span::Piece(&second)], vec![]).unwrap();
    cs.store().sync().unwrap();
    // Dropping without close() is the abandoned session: there is deliberately no Drop that
    // checkpoints, so the container learns nothing and the working directory stays behind. The
    // kernel releases the fold's flock the same way it would on a real crash.
    drop(cs);
    assert!(hot.exists(), "the fixture must leave a working directory behind");

    // The container alone has only the first record — proving the second exists solely in hot.
    let stale = open_read_container(&ct, cfg()).unwrap();
    assert!(stale.reconstruct("kept:2").unwrap().is_none(), "the fixture must be a real gap");
    drop(stale);

    // Resuming must adopt that directory, not materialize over it: re-materializing would replace
    // acknowledged writes with an older committed snapshot.
    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    assert_eq!(
        cs.store().reconstruct("kept:2").unwrap().unwrap(),
        second,
        "an acknowledged write must survive the abandoned session"
    );
    cs.close().unwrap();

    let rs = open_read_container(&ct, cfg()).unwrap();
    assert_eq!(rs.reconstruct("kept:1").unwrap().unwrap(), first);
    assert_eq!(rs.reconstruct("kept:2").unwrap().unwrap(), second);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn reclaim_returns_the_space_repeated_checkpoints_leak() {
    use turndb::container::reclaim;
    use turndb::store::ContainerStore;

    let root = tmp("reclaim");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("grows.turndb");

    // Every session restages MANIFEST and the sidecars, so waste accumulates whether or not the
    // store grows. Ten sessions is a fortnight of daily checkpoints, not an abusive fixture.
    let mut want = Vec::new();
    for round in 0..10 {
        let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
        let id = format!("r:{round:02}");
        let body = noise(round, 1200);
        cs.store().put(&id, &[Span::Piece(&body)], vec![]).unwrap();
        want.push((id, body));
        cs.close().unwrap();
    }

    let before = Container::open(&ct).unwrap();
    let waste = before.free_bytes();
    let live = before.member_bytes();
    let members = before.len();
    assert!(waste > 0, "repeated checkpoints must leave superseded extents to reclaim");
    drop(before);

    let stats = reclaim(&ct).unwrap();
    assert_eq!(stats.members, members, "reclaim carries every member across");
    assert!(stats.reclaimed > 0, "reclaim must return space: {stats:?}");
    assert!(stats.bytes_after < stats.bytes_before, "the file must shrink: {stats:?}");

    // The point of the exercise: the waste is gone and the content is not.
    let after = Container::open(&ct).unwrap();
    assert_eq!(after.free_bytes(), 0, "nothing is superseded in a freshly written container");
    assert_eq!(after.member_bytes(), live, "live bytes are unchanged");
    assert_eq!(after.verify().unwrap(), members);
    drop(after);

    let rs = open_read_container(&ct, cfg()).unwrap();
    for (id, body) in &want {
        assert_eq!(rs.reconstruct(id).unwrap().unwrap(), *body, "{id} must survive the rewrite");
    }
    drop(rs);

    // Idempotent: a container with nothing to reclaim is left exactly as it is.
    let again = reclaim(&ct).unwrap();
    assert_eq!(again.reclaimed, 0, "a clean container has nothing to return: {again:?}");
    assert_eq!(again.bytes_after, again.bytes_before);

    // And it stays writable afterwards.
    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    cs.store().put("after:1", &[Span::Piece(b"still writable")], vec![]).unwrap();
    cs.close().unwrap();
    assert_eq!(
        open_read_container(&ct, cfg()).unwrap().reconstruct("after:1").unwrap().unwrap(),
        b"still writable"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn reclaim_refuses_a_container_a_writer_may_be_holding() {
    use turndb::container::reclaim;
    use turndb::store::ContainerStore;

    let root = tmp("reclaim-busy");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("busy.turndb");

    let mut cs = ContainerStore::open(&ct, cfg()).unwrap();
    cs.store().put("held:1", &[Span::Piece(b"in flight")], vec![]).unwrap();
    cs.store().sync().unwrap();

    // Rewriting now would publish a container about to be superseded by a checkpoint of writes it
    // never saw — and the writer's own working directory is the evidence that is happening.
    let err = match reclaim(&ct) {
        Ok(s) => panic!("reclaim must refuse a container with a live writer, got {s:?}"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("working directory"), "the refusal must name why, got: {err}");

    cs.close().unwrap();
    reclaim(&ct).expect("once the writer is gone, reclaim proceeds");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn space_accounting_answers_for_a_single_file_store_too() {
    let root = tmp("space");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    build_store(&dir);

    // The directory's own answer is the reference: whatever it reports, the same store served out
    // of a file must report too. A fold that cannot measure itself returns zero rather than
    // failing, so this is the shape of bug that hides in a report nobody cross-checks.
    let from_dir = Store::open_read(&dir, cfg()).unwrap();
    let want = from_dir.fold().disk_bytes();
    assert!(want > 0, "the fixture must have fold bytes to account for");
    drop(from_dir);

    let ct = root.join("space.turndb");
    checkpoint_into_container(&dir, &ct).unwrap();
    let from_container = open_read_container(&ct, cfg()).unwrap();
    assert_eq!(
        from_container.fold().disk_bytes(),
        want,
        "a container-backed fold must account for the same bytes as the directory it came from"
    );
    drop(from_container);

    let pk = root.join("space.pack");
    turndb::pack::write(&dir, &pk).unwrap();
    let from_pack = turndb::store::open_read_pack(&pk, cfg()).unwrap();
    assert_eq!(
        from_pack.fold().disk_bytes(),
        want,
        "a pack-backed fold must account for the same bytes too"
    );
    std::fs::remove_dir_all(&root).ok();
}
