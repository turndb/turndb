//! The container gate: a store in one MUTABLE file answers exactly as the directory did, grows
//! without invalidating what a reader already resolved, and survives a torn commit.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use turndb::container::{
    Container, ContainerReader, ALIGN, CONTAINER_VERSION, MAGIC, REGION_START, SLOT_LEN,
};
use turndb::fold::FoldCfg;
use turndb::readat::ReadAt as _;
use turndb::store::{convert_to_file, open_read_container, Span, Store};
use turndb::AttrValue;

#[derive(Clone)]
struct CountingSource {
    bytes: Arc<Vec<u8>>,
    reads: Arc<Mutex<Vec<(u64, usize)>>>,
}

impl CountingSource {
    fn new(bytes: Vec<u8>) -> CountingSource {
        CountingSource { bytes: Arc::new(bytes), reads: Arc::new(Mutex::new(Vec::new())) }
    }

    fn reads(&self) -> Vec<(u64, usize)> {
        self.reads.lock().unwrap().clone()
    }
}

impl turndb::readat::ReadAt for CountingSource {
    fn read_exact_at(&self, into: &mut [u8], offset: u64) -> io::Result<()> {
        let at = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        let bytes = self
            .bytes
            .get(at..at.saturating_add(into.len()))
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past counting source"))?;
        into.copy_from_slice(bytes);
        self.reads.lock().unwrap().push((offset, into.len()));
        Ok(())
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }
}

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

/// Materialize the checked-in 0.1.3 directory-store fixture — the retired layout as its last
/// writer actually left it, non-empty WAL included. See tests/fixtures/directory-store-0.1.3.hex.
fn unpack_fixture(into: &Path) {
    let hex_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/directory-store-0.1.3.hex");
    let text = std::fs::read_to_string(&hex_path).unwrap();
    let mut name: Option<PathBuf> = None;
    let mut hex = String::new();
    let flush = |name: &Option<PathBuf>, hex: &str| {
        if let Some(path) = name {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    };
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("== ") {
            flush(&name, &hex);
            hex.clear();
            let rel = rest.split_whitespace().next().unwrap();
            name = Some(into.join(rel));
        } else {
            hex.push_str(line.trim());
        }
    }
    flush(&name, &hex);
}

/// The generator's body function, byte for byte (xorshift64 over the seed).
fn fixture_body(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

fn fold_payload_ranges(container: &Container) -> Vec<std::ops::Range<u64>> {
    let mut ranges = Vec::new();
    for name in container.names().filter(|name| name.ends_with(".fold")) {
        let mut header_left = turndb::fold::segment::SEG_HDR_LEN;
        for (offset, length) in container.member_extents(name).unwrap() {
            let header = header_left.min(length);
            header_left -= header;
            if length > header {
                ranges.push(offset + header..offset + length);
            }
        }
        assert_eq!(header_left, 0, "segment {name} is shorter than its header");
    }
    ranges
}

fn overlaps(read: &(u64, usize), range: &std::ops::Range<u64>) -> bool {
    let read_end = read.0 + read.1 as u64;
    read.0 < range.end && range.start < read_end
}

#[test]
fn an_empty_cold_open_reads_only_the_superblock_slots() {
    let root = tmp("empty-cold-open");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("store.turndb");
    Store::open_file(&path, cfg()).unwrap().close().unwrap();

    let source = CountingSource::new(std::fs::read(&path).unwrap());
    turndb::store::open_read_container_source(
        Arc::new(source.clone()),
        "memory://empty-cold-open",
        cfg(),
        turndb::read_limits::ReadLimits::default(),
    )
    .unwrap();
    assert_eq!(source.reads(), vec![(0, 4096), (4096, 4096)]);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn cold_open_reads_only_metadata_while_a_missing_sidecar_remains_advisory() {
    let root = tmp("cold-open-reads");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("store.turndb");
    let mut store = Store::open_file(&path, cfg()).unwrap();
    for round in 0..3u64 {
        for record in 0..4u64 {
            store
                .put_body(
                    &format!("trace/{round}/{record}"),
                    &fixture_body(round * 10 + record + 1, 6 * 1024),
                    vec![],
                )
                .unwrap();
        }
        store.sync().unwrap();
        store.flush().unwrap();
    }
    store.close().unwrap();

    let container = Container::open(&path).unwrap();
    let segment_names: Vec<String> =
        container.names().filter(|name| name.ends_with(".fold")).map(String::from).collect();
    let part_count = container.names().filter(|name| name.ends_with(".part")).count();
    let dictionary_count = container.names().filter(|name| name.ends_with(".zd")).count();
    assert!(segment_names.len() > 1, "fixture must exercise sealed and active segments");
    assert!(part_count > 1, "fixture must exercise a multi-part cold open");
    for segment in &segment_names {
        let sidecar = segment.replace(".fold", ".dir");
        assert!(
            container.contains(&sidecar),
            "every committed segment, including the active one, needs open metadata: {sidecar}"
        );
    }
    let payload_ranges = fold_payload_ranges(&container);
    drop(container);

    let source = CountingSource::new(std::fs::read(&path).unwrap());
    let reader = turndb::store::open_read_container_source(
        Arc::new(source.clone()),
        "memory://cold-open",
        cfg(),
        turndb::read_limits::ReadLimits::default(),
    )
    .unwrap();
    let cold_reads = source.reads();
    assert_eq!(
        cold_reads.len(),
        4 + 2 * segment_names.len() + 2 * part_count + dictionary_count,
        "two slots, directory, manifest, one sidecar and header per segment, one whole read per dictionary, and footer plus TOC per part: {cold_reads:?}"
    );
    assert!(
        cold_reads.iter().all(|read| payload_ranges.iter().all(|payload| !overlaps(read, payload))),
        "a valid current container must not fetch fold payload merely to open: {cold_reads:?}"
    );
    assert_eq!(reader.reconstruct("trace/2/3").unwrap().unwrap().len(), 6 * 1024);

    // Sidecars stay advisory format data. Removing the active one makes open slower, not invalid:
    // the reader reconstructs its directory from the checksummed block frames and answers exactly.
    let active = segment_names.last().unwrap();
    let active_sidecar = active.replace(".fold", ".dir");
    let mut container = Container::open(&path).unwrap();
    assert!(container.remove(&active_sidecar).unwrap());
    container.commit().unwrap();
    let payload_ranges = fold_payload_ranges(&container);
    drop(container);

    let fallback = CountingSource::new(std::fs::read(&path).unwrap());
    let reader = turndb::store::open_read_container_source(
        Arc::new(fallback.clone()),
        "memory://cold-open-without-advisory-sidecar",
        cfg(),
        turndb::read_limits::ReadLimits::default(),
    )
    .unwrap();
    let fallback_reads = fallback.reads();
    assert!(
        fallback_reads
            .iter()
            .any(|read| payload_ranges.iter().any(|payload| overlaps(read, payload))),
        "without advisory open metadata the reader must derive it from the active payload"
    );
    assert_eq!(reader.reconstruct("trace/2/3").unwrap().unwrap().len(), 6 * 1024);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_retired_directory_store_converts_whole_including_its_unflushed_wal() {
    let root = tmp("convert-dir");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    unpack_fixture(&dir);
    assert!(
        std::fs::metadata(dir.join("WAL")).unwrap().len() > 0,
        "the fixture must carry an acknowledged, unflushed record in its WAL"
    );

    let ct = root.join("store.turndb");
    let stats = convert_to_file(&dir, &ct).unwrap();
    assert!(stats.members > 3, "manifest + parts + segments: {stats:?}");
    assert_eq!(Container::open(&ct).unwrap().verify().unwrap(), stats.members);

    let rs = open_read_container(&ct, cfg()).unwrap();
    for round in 0..2u64 {
        for i in 0..6u64 {
            let id = format!("fix:{round}:{i}");
            if id == "fix:0:0" {
                assert!(
                    rs.reconstruct(&id).unwrap().is_none(),
                    "the delete must hold through conversion"
                );
                continue;
            }
            let mut want = b"[".to_vec();
            want.extend_from_slice(&fixture_body(round * 10 + i, 1800));
            want.extend_from_slice(b"]");
            assert_eq!(
                rs.reconstruct(&id).unwrap().unwrap(),
                want,
                "{id} must reconstruct byte-exact out of the converted file"
            );
            let record = rs.get(&id).unwrap().unwrap();
            assert_eq!(
                record.attrs.iter().find(|(k, _)| k == "model").map(|(_, v)| v.clone()),
                Some(AttrValue::Str(format!("m{}", i % 2))),
                "{id} scalar metadata must survive"
            );
        }
    }
    // The record that lived ONLY in the WAL: synced by 0.1.3, never flushed. Conversion opens
    // the writer role, which replays it — losing it would be losing an acknowledged write.
    assert_eq!(
        rs.reconstruct("fix:wal:only").unwrap().unwrap(),
        fixture_body(999, 700),
        "conversion must replay the acknowledged WAL record"
    );
    drop(rs);

    // And the converted file is an ordinary live store: writable, and done with the directory.
    let mut s = Store::open_file(&ct, cfg()).unwrap();
    s.put("after:convert", &[Span::Piece(b"life goes on")], vec![]).unwrap();
    s.sync().unwrap();
    s.close().unwrap();
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
    // The waste is the superseded member extent plus the first commit's directory, which the
    // second commit superseded — a directory is bytes like any other, and leaving it uncounted
    // is how dead space becomes unaccountable.
    let staged = c.free_bytes();
    assert!(staged > 5000, "superseded member and directory extents are waste: {staged}");
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

/// A positioned source whose first answer to the length question is from before the last commit
/// and every later answer is honest. That is exactly the view a lock-free open gets when a writer
/// commits between the open's length query and its superblock read.
struct StaleLenSource {
    bytes: Arc<Vec<u8>>,
    stale: Mutex<Option<u64>>,
}

impl turndb::readat::ReadAt for StaleLenSource {
    fn read_exact_at(&self, into: &mut [u8], offset: u64) -> io::Result<()> {
        let at = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        let bytes = self
            .bytes
            .get(at..at.saturating_add(into.len()))
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past stale source"))?;
        into.copy_from_slice(bytes);
        Ok(())
    }

    fn len(&self) -> io::Result<u64> {
        match self.stale.lock().unwrap().take() {
            Some(stale) => Ok(stale),
            None => Ok(self.bytes.len() as u64),
        }
    }
}

/// A lock-free open measures the container's length and then reads the superblock slots, and a
/// writer can commit in that gap: bytes land past the old tail, fsync, slot flip. The newest
/// slot's tail then exceeds the stale measurement with nothing truncated, so the open must
/// re-measure and serve the committed state in full. A length that stays short of the tail on the
/// second answer is genuine truncation and must still refuse — both questions below, per the
/// testing standard: the nearest valid thing is accepted whole, the invalid one still stops.
#[test]
fn a_commit_between_the_length_query_and_the_slot_read_is_not_truncation() {
    let root = tmp("stalelen");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("stale.turndb");

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("early", &noise(1, 3000)).unwrap();
    c.commit().unwrap();
    drop(c);
    let stale_len = std::fs::metadata(&ct).unwrap().len();

    let mut c = Container::open(&ct).unwrap();
    c.put_bytes("late", &noise(2, 5000)).unwrap();
    c.commit().unwrap();
    drop(c);
    let bytes = std::fs::read(&ct).unwrap();

    // The race's precondition, asserted rather than assumed: the newest committed tail lies
    // beyond the length a reader could have measured before the second commit.
    let live = newest_slot(&bytes);
    let tail = u64::from_le_bytes(bytes[live + 40..live + 48].try_into().unwrap());
    assert!(tail > stale_len, "the fixture must put the committed tail past the stale length");

    // Accept the nearest valid thing, and accept it whole: every committed member, byte-exact,
    // not merely an open that returns.
    let racing = Arc::new(StaleLenSource {
        bytes: Arc::new(bytes.clone()),
        stale: Mutex::new(Some(stale_len)),
    });
    let r = ContainerReader::open(racing, "stale.turndb").unwrap_or_else(|e| {
        panic!("a commit racing the length query must not read as truncation: {e}")
    });
    assert_eq!(r.names().collect::<Vec<_>>(), ["early", "late"]);
    assert_eq!(r.read_file_bounded("early", 1 << 20).unwrap(), noise(1, 3000));
    assert_eq!(r.read_file_bounded("late", 1 << 20).unwrap(), noise(2, 5000));

    // Genuine truncation answers short on the second measurement too, and must still refuse —
    // through the source reader and the file open alike, and for the stated reason rather than
    // some downstream parse failure.
    let cut = &bytes[..stale_len as usize];
    let steady =
        Arc::new(StaleLenSource { bytes: Arc::new(cut.to_vec()), stale: Mutex::new(None) });
    let err = match ContainerReader::open(steady, "cut.turndb") {
        Ok(_) => panic!("a source truly missing its committed tail must refuse"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("truncated"), "the refusal must name truncation, got: {err}");
    let cut_path = root.join("cut.turndb");
    std::fs::write(&cut_path, cut).unwrap();
    let err = match Container::open(&cut_path) {
        Ok(_) => panic!("a file truly missing its committed tail must refuse"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("truncated"), "the refusal must name truncation, got: {err}");
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
fn a_session_that_writes_nothing_still_leaves_a_container_that_opens() {
    let root = tmp("empty-writer");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("empty.turndb");

    // Applying no records is not an error. `turndb import new.turndb input.jsonl` reaches exactly
    // this state whenever every line of the input is skipped — a mistyped schema, an empty file —
    // and the file left behind must open as an empty store, not refuse as a memberless husk.
    let s = Store::open_file(&ct, cfg()).unwrap();
    s.close().unwrap();
    assert!(ct.is_file(), "the container must exist");

    let rs = open_read_container(&ct, cfg()).unwrap();
    assert!(rs.ids().unwrap().is_empty(), "an empty store holds no ids");
    drop(rs);

    // And it must take writes afterwards rather than stay poisoned by its own first session.
    let mut s = Store::open_file(&ct, cfg()).unwrap();
    let body = noise(7, 900);
    s.put("later", &[Span::Piece(&body)], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();

    let rs = open_read_container(&ct, cfg()).unwrap();
    assert_eq!(rs.reconstruct("later").unwrap().unwrap(), body, "the later write must survive");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_committed_tail_inside_a_sealed_segment_is_refused_not_rolled_back() {
    use std::sync::Arc;
    use turndb::fold::{Fold, FoldTail};
    use turndb::readat::ReadAt;

    let root = tmp("sealed-rollback");
    std::fs::create_dir_all(&root).unwrap();
    let fold_dir = root.join("fold");

    // A fold several segments deep, so there is a sealed one to aim a tail into.
    {
        let mut f = Fold::open(&fold_dir, cfg()).unwrap();
        for i in 0..24u64 {
            f.put(&noise(i, 2000)).unwrap();
        }
        f.sync().unwrap();
    }
    let mut segs: Vec<u32> = std::fs::read_dir(&fold_dir)
        .unwrap()
        .flatten()
        .filter_map(|e| turndb::fold::segment::parse_seg_name(&e.file_name().to_string_lossy()))
        .collect();
    segs.sort_unstable();
    assert!(segs.len() >= 3, "the fixture must roll several segments: {segs:?}");

    // Seal everything below the last one: hand it in as a reader and take it out of the directory,
    // which is exactly the shape a container-backed open produces.
    // An open handle outlives the unlink on Unix, which is how a reader keeps addressing bytes the
    // directory no longer names — the same property a container extent has.
    let last = *segs.last().unwrap();
    let mut sealed: Vec<Arc<dyn ReadAt>> = Vec::new();
    for n in 0..last {
        let path = fold_dir.join(turndb::fold::segment::seg_name(n));
        let file = std::fs::File::open(&path).unwrap();
        sealed.push(Arc::new(file) as Arc<dyn ReadAt>);
        std::fs::remove_file(&path).unwrap();
    }

    // A tail inside a sealed segment means whatever supplied them is AHEAD of the manifest: state
    // was sealed that was never committed. Rolling back would mean unlinking a segment that is not
    // a file, so this must say so rather than try.
    let refused = Fold::open_at_over_with_limits(
        &fold_dir,
        cfg(),
        Some(FoldTail { seg: 0, off: 64 }),
        &[],
        sealed.clone(),
        Default::default(),
    );
    match refused {
        Ok(_) => panic!("a committed tail below the sealed floor must be refused"),
        Err(e) => {
            let text = format!("{e:#}");
            assert!(
                text.contains("sealed") && text.contains("ahead of the manifest"),
                "the refusal must name the disagreement it found: {text}"
            );
        }
    }

    // The same open without that disagreement is the ordinary case and must still work.
    if let Err(e) =
        Fold::open_at_over_with_limits(&fold_dir, cfg(), None, &[], sealed, Default::default())
    {
        panic!("a fold over sealed segments must open: {e:#}");
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn reclaim_returns_the_space_repeated_sessions_leak() {
    use turndb::container::reclaim;

    let root = tmp("reclaim");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("grows.turndb");

    // Every flush restages MANIFEST and the live segment, so waste accumulates whether or not
    // the store grows. Ten sessions is a fortnight of daily use, not an abusive fixture.
    let mut want = Vec::new();
    for round in 0..10 {
        let mut s = Store::open_file(&ct, cfg()).unwrap();
        let id = format!("r:{round:02}");
        let body = noise(round, 1200);
        s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
        want.push((id, body));
        s.sync().unwrap();
        s.flush().unwrap();
        s.close().unwrap();
    }

    let before = Container::open(&ct).unwrap();
    let waste = before.free_bytes();
    let live = before.member_bytes();
    let members = before.len();
    assert!(waste > 0, "repeated sessions must leave superseded extents to reclaim");
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
    let mut s = Store::open_file(&ct, cfg()).unwrap();
    s.put("after:1", &[Span::Piece(b"still writable")], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();
    assert_eq!(
        open_read_container(&ct, cfg()).unwrap().reconstruct("after:1").unwrap().unwrap(),
        b"still writable"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn reclaim_refuses_a_container_a_writer_may_be_holding() {
    use turndb::container::reclaim;

    let root = tmp("reclaim-busy");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("busy.turndb");

    let mut s = Store::open_file(&ct, cfg()).unwrap();
    s.put("held:1", &[Span::Piece(b"in flight")], vec![]).unwrap();
    s.sync().unwrap();

    // Reclaim publishes by renaming over the live name; a writer holding the flock would keep
    // committing to the old inode, so contention must refuse — typed, the way a second writer's
    // open refuses.
    let err = match reclaim(&ct) {
        Ok(stats) => panic!("reclaim must refuse a container with a live writer, got {stats:?}"),
        Err(e) => e,
    };
    assert!(
        err.downcast_ref::<turndb::fold::WriterLocked>().is_some(),
        "the refusal must be the typed contention error: {err:#}"
    );

    // Closed but NOT settled: sync() then a plain drop leaves the acknowledged record in the WAL
    // sidecar, and rewriting under it would strand what only its writer can settle.
    drop(s);
    let wal = {
        let mut p = ct.clone().into_os_string();
        p.push("-wal");
        PathBuf::from(p)
    };
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 0,
        "the fixture must leave an unsettled WAL sidecar"
    );
    let err = match reclaim(&ct) {
        Ok(stats) => panic!("reclaim must refuse an unsettled WAL, got {stats:?}"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("write-ahead log"), "the refusal must name why, got: {err}");

    // Settle it properly and reclaim proceeds.
    let mut s = Store::open_file(&ct, cfg()).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();
    reclaim(&ct).expect("once the writer is gone and the WAL settled, reclaim proceeds");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn space_accounting_answers_for_a_single_file_store_too() {
    let root = tmp("space");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("space.turndb");

    // The writer's own answer is the reference: whatever the live session reports, the same
    // store served back out of the container's members must report too. A fold that cannot
    // measure itself returns zero rather than failing, so this is the shape of bug that hides
    // in a report nobody cross-checks.
    let mut s = Store::open_file(&ct, cfg()).unwrap();
    for round in 0..3u64 {
        for i in 0..12u64 {
            s.put(
                &format!("r{round}:{i:02}"),
                &[Span::Piece(&noise(round * 100 + i, 1800))],
                vec![],
            )
            .unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    assert!(s.fold().segment_count() > 1, "the fixture must roll at least one segment");
    let want = s.fold().disk_bytes();
    assert!(want > 0, "the fixture must have fold bytes to account for");
    s.close().unwrap();

    let from_container = open_read_container(&ct, cfg()).unwrap();
    assert_eq!(
        from_container.fold().disk_bytes(),
        want,
        "a container-backed fold must account for the same bytes the live session did"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_member_grows_across_commits_without_being_copied() {
    let root = tmp("grow-member");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("grow-member.turndb");

    let first = noise(1, 3000);
    let second = noise(2, 2000);
    let third = noise(3, 1000);
    let fourth = noise(4, 1000);
    let fill = |src: &[u8]| {
        let src = src.to_vec();
        move |at: u64, into: &mut [u8]| {
            into.copy_from_slice(&src[at as usize..at as usize + into.len()]);
            Ok(())
        }
    };

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("fold/seg-00000000.fold", &first).unwrap();
    c.commit().unwrap();

    // A commit intervened (its directory landed past the member), so this extension cannot
    // coalesce: the member gains an extent instead of being rewritten.
    c.append_stream("fold/seg-00000000.fold", second.len() as u64, fill(&second)).unwrap();
    c.commit().unwrap();

    // Two extensions with nothing between them coalesce into one extent.
    c.append_stream("fold/seg-00000000.fold", third.len() as u64, fill(&third)).unwrap();
    c.append_stream("fold/seg-00000000.fold", fourth.len() as u64, fill(&fourth)).unwrap();
    c.commit().unwrap();

    let mut want = first.clone();
    want.extend_from_slice(&second);
    want.extend_from_slice(&third);
    want.extend_from_slice(&fourth);

    let extents = c.member_extents("fold/seg-00000000.fold").unwrap();
    assert_eq!(extents.len(), 3, "one extent per commit that extended it: {extents:?}");
    for &(off, _) in &extents {
        assert_eq!(off % ALIGN, 0, "every fresh extent starts on a page: {extents:?}");
    }
    assert_eq!(extents[0].0, REGION_START, "the first member starts the region");
    assert_eq!(c.read_file_bounded("fold/seg-00000000.fold", 1 << 20).unwrap(), want);
    // The combined checksum must equal a checksum of the logical bytes, or verify would pass on
    // writes and fail on reopen — the combine is the thing under test here.
    assert_eq!(c.verify().unwrap(), 1);
    drop(c);

    let reopened = Container::open(&ct).unwrap();
    assert_eq!(reopened.member_extents("fold/seg-00000000.fold").unwrap(), extents);
    assert_eq!(reopened.read_file_bounded("fold/seg-00000000.fold", 1 << 20).unwrap(), want);
    assert_eq!(reopened.verify().unwrap(), 1);

    // The scattered member still opens through the reader seam like any contiguous one.
    let reader = reopened.extent("fold/seg-00000000.fold").unwrap();
    assert_eq!(reader.len().unwrap(), want.len() as u64);
    let mut tail_bytes = vec![0u8; 1500];
    reader.read_exact_at(&mut tail_bytes, want.len() as u64 - 1500).unwrap();
    assert_eq!(tail_bytes, want[want.len() - 1500..]);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_extension_after_an_intervening_member_takes_a_fresh_aligned_extent() {
    let root = tmp("interleave");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("interleave.turndb");

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("a", &noise(1, 100)).unwrap();
    // Nothing landed since: the extension coalesces and the member stays one extent.
    c.append_stream("a", 50, |at, into| {
        into.copy_from_slice(&noise(2, 50)[at as usize..at as usize + into.len()]);
        Ok(())
    })
    .unwrap();
    assert_eq!(c.member_extents("a").unwrap().len(), 1, "adjacent extension must coalesce");

    // A member landed between: the next extension cannot coalesce and must not overwrite it.
    c.put_bytes("b", &noise(3, 100)).unwrap();
    c.append_stream("a", 50, |at, into| {
        into.copy_from_slice(&noise(4, 50)[at as usize..at as usize + into.len()]);
        Ok(())
    })
    .unwrap();
    let extents = c.member_extents("a").unwrap();
    assert_eq!(extents.len(), 2, "an intervening member forces a fresh extent");
    assert_eq!(extents[1].0 % ALIGN, 0, "the fresh extent starts on a page");
    c.commit().unwrap();

    let mut want_a = noise(1, 100);
    want_a.extend_from_slice(&noise(2, 50));
    want_a.extend_from_slice(&noise(4, 50));
    assert_eq!(c.read_file_bounded("a", 1024).unwrap(), want_a);
    assert_eq!(c.read_file_bounded("b", 1024).unwrap(), noise(3, 100));
    assert_eq!(c.verify().unwrap(), 2);

    // Extending a member that does not exist is a refusal, not a creation.
    assert!(c.append_stream("nope", 1, |_, _| Ok(())).is_err());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn revision_two_finalization_bit_is_ignored_and_retired_on_write() {
    let root = tmp("v2-final-bit");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("old.turndb");

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("kept", b"payload").unwrap();
    c.commit().unwrap();
    drop(c);

    // Recast the live slot as revision 2 carrying its historical finalization bit. The directory
    // representation is the same, so only the version, bit, and checksum change.
    let mut bytes = std::fs::read(&ct).unwrap();
    let live = newest_slot(&bytes);
    let mut current_with_reserved_bit = bytes.clone();
    current_with_reserved_bit[live + 50] = 1;
    let digest = blake3::hash(&current_with_reserved_bit[live..live + 52]);
    current_with_reserved_bit[live + 52..live + 56].copy_from_slice(&digest.as_bytes()[0..4]);
    let bad_current = root.join("bad-current.turndb");
    std::fs::write(&bad_current, current_with_reserved_bit).unwrap();
    let error = match Container::open(&bad_current) {
        Ok(_) => panic!("revision 3 must refuse a nonzero reserved byte"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("reserved bits"), "wrong revision-3 refusal: {error}");

    bytes[live + 49] = 2;
    bytes[live + 50] = 1;
    let digest = blake3::hash(&bytes[live..live + 52]);
    bytes[live + 52..live + 56].copy_from_slice(&digest.as_bytes()[0..4]);
    std::fs::write(&ct, &bytes).unwrap();

    let mut unknown_v2 = bytes.clone();
    unknown_v2[live + 50] = 2;
    let digest = blake3::hash(&unknown_v2[live..live + 52]);
    unknown_v2[live + 52..live + 56].copy_from_slice(&digest.as_bytes()[0..4]);
    let bad_v2 = root.join("bad-v2.turndb");
    std::fs::write(&bad_v2, unknown_v2).unwrap();
    let error = match Container::open(&bad_v2) {
        Ok(_) => panic!("revision 2 must still refuse unknown flag bits"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("revision-2 flags"), "wrong revision-2 refusal: {error}");

    // The old bit carries no lifecycle semantics: reads and writes both work. The first commit
    // publishes revision 3 and restores bytes 50..52 to their reserved-zero state.
    let mut c = Container::open(&ct).unwrap();
    assert_eq!(c.read_file_bounded("kept", 64).unwrap(), b"payload");
    c.put_bytes("more", b"continued").unwrap();
    c.commit().unwrap();
    drop(c);

    let bytes = std::fs::read(&ct).unwrap();
    let live = newest_slot(&bytes);
    assert_eq!(bytes[live + 49], CONTAINER_VERSION);
    assert_eq!(&bytes[live + 50..live + 52], &[0, 0]);
    let r = Container::open(&ct).unwrap();
    assert_eq!(r.read_file_bounded("more", 64).unwrap(), b"continued");
    std::fs::remove_dir_all(&root).ok();
}

/// The first published containers carried single-extent members and an unstamped free list. They
/// must open exactly as written, and the first commit over one publishes the current revision —
/// an upgrade the owner performs by writing, not a migration tool.
#[test]
fn the_first_published_revision_upgrades_on_first_commit() {
    let root = tmp("first-revision");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("old.turndb");

    fn vput(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }

    // Hand-build the old layout: two packed members with a dead gap between them (its free list
    // recorded extents as bare pairs), then the directory, then a version-1 superblock.
    let alpha = b"legacy payload".to_vec();
    let beta = noise(9, 20);
    let alpha_off = REGION_START;
    let dead_off = alpha_off + alpha.len() as u64;
    let beta_off = dead_off + 10;
    let dir_off = beta_off + beta.len() as u64;

    let mut payload = Vec::new();
    vput(&mut payload, 2);
    for (name, off, bytes) in [("alpha", alpha_off, &alpha), ("beta", beta_off, &beta)] {
        vput(&mut payload, name.len() as u64);
        payload.extend_from_slice(name.as_bytes());
        vput(&mut payload, off);
        vput(&mut payload, bytes.len() as u64);
        payload.extend_from_slice(&crc32fast::hash(bytes).to_le_bytes());
    }
    vput(&mut payload, 1);
    vput(&mut payload, dead_off);
    vput(&mut payload, 10);

    let tail = dir_off + payload.len() as u64;
    let mut slot = [0u8; SLOT_LEN as usize];
    slot[0..8].copy_from_slice(MAGIC);
    slot[8..16].copy_from_slice(&1u64.to_le_bytes()); // seq
    slot[16..24].copy_from_slice(&dir_off.to_le_bytes());
    slot[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    slot[28..32].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    slot[32..36].copy_from_slice(&2u32.to_le_bytes()); // n_entries
    slot[36..40].copy_from_slice(&crc32fast::hash(&payload).to_le_bytes());
    slot[40..48].copy_from_slice(&tail.to_le_bytes());
    slot[48] = 0; // stored
    slot[49] = 1; // the first published revision
    let digest = blake3::hash(&slot[0..52]);
    slot[52..56].copy_from_slice(&digest.as_bytes()[0..4]);

    let mut file = vec![0u8; REGION_START as usize];
    file[0..SLOT_LEN as usize].copy_from_slice(&slot);
    file.extend_from_slice(&alpha);
    file.extend_from_slice(&noise(0, 10)); // the dead extent's bytes
    file.extend_from_slice(&beta);
    file.extend_from_slice(&payload);
    std::fs::write(&ct, &file).unwrap();

    // It opens exactly as written: members, bytes, and the waste it already carried.
    let mut c = Container::open(&ct).unwrap();
    assert_eq!(c.read_file_bounded("alpha", 64).unwrap(), alpha);
    assert_eq!(c.read_file_bounded("beta", 64).unwrap(), beta);
    assert_eq!(c.free_bytes(), 10, "the old free list must round-trip");
    assert_eq!(c.verify().unwrap(), 2);

    // Writing to it publishes the current revision; nothing it held is disturbed.
    let delta = noise(11, 30);
    c.append_stream("alpha", delta.len() as u64, |at, into| {
        into.copy_from_slice(&delta[at as usize..at as usize + into.len()]);
        Ok(())
    })
    .unwrap();
    let seq = c.commit().unwrap();
    assert_eq!(seq, 2);
    drop(c);

    let bytes = std::fs::read(&ct).unwrap();
    let live = newest_slot(&bytes);
    assert_eq!(bytes[live + 49], CONTAINER_VERSION, "a commit publishes the current revision");

    let reopened = Container::open(&ct).unwrap();
    let mut want = alpha.clone();
    want.extend_from_slice(&delta);
    assert_eq!(reopened.read_file_bounded("alpha", 64).unwrap(), want);
    assert_eq!(reopened.read_file_bounded("beta", 64).unwrap(), beta);
    assert!(reopened.free_bytes() >= 10, "old waste stays answerable after the upgrade");
    assert_eq!(reopened.verify().unwrap(), 2);
    std::fs::remove_dir_all(&root).ok();
}

/// A checksum-valid directory that lies about layout must refuse at open, before any read can be
/// served bytes that are simultaneously someone else's. The random storm reaches these shapes by
/// luck; this reaches each one on purpose.
#[test]
fn a_directory_that_lies_about_layout_is_refused_at_open() {
    let root = tmp("layout-lies");
    std::fs::create_dir_all(&root).unwrap();

    fn vput(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }

    // One real member so the file has bytes to lie about.
    let body = noise(5, 4096);
    let member_off = REGION_START;

    // Build a container around a hand-encoded directory and return the open error.
    let build = |tag: &str, payload: &[u8], n_entries: u32, seq: u64| -> String {
        let dir_off = member_off + body.len() as u64;
        let tail = dir_off + payload.len() as u64;
        let mut slot = [0u8; SLOT_LEN as usize];
        slot[0..8].copy_from_slice(MAGIC);
        slot[8..16].copy_from_slice(&seq.to_le_bytes());
        slot[16..24].copy_from_slice(&dir_off.to_le_bytes());
        slot[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        slot[28..32].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        slot[32..36].copy_from_slice(&n_entries.to_le_bytes());
        slot[36..40].copy_from_slice(&crc32fast::hash(payload).to_le_bytes());
        slot[40..48].copy_from_slice(&tail.to_le_bytes());
        slot[48] = 0; // stored
        slot[49] = CONTAINER_VERSION;
        let digest = blake3::hash(&slot[0..52]);
        slot[52..56].copy_from_slice(&digest.as_bytes()[0..4]);

        let mut file = vec![0u8; REGION_START as usize];
        file[0..SLOT_LEN as usize].copy_from_slice(&slot);
        file.extend_from_slice(&noise(5, 4096));
        file.extend_from_slice(payload);
        let path = root.join(format!("{tag}.turndb"));
        std::fs::write(&path, &file).unwrap();
        match Container::open(&path) {
            Ok(_) => panic!("{tag}: a lying directory must refuse at open"),
            Err(e) => format!("{e:#}"),
        }
    };

    // Two members claiming overlapping extents.
    let mut p = Vec::new();
    vput(&mut p, 2);
    for (name, off, len) in [("first", member_off, 4096u64), ("second", member_off + 100, 200u64)] {
        vput(&mut p, name.len() as u64);
        p.extend_from_slice(name.as_bytes());
        vput(&mut p, 1); // n_extents
        vput(&mut p, off);
        vput(&mut p, len);
        p.extend_from_slice(&0u32.to_le_bytes());
    }
    vput(&mut p, 0);
    assert!(build("member-overlap", &p, 2, 1).contains("overlapping"));

    // A free extent under a live member's bytes.
    let mut p = Vec::new();
    vput(&mut p, 1);
    vput(&mut p, 5);
    p.extend_from_slice(b"whole");
    vput(&mut p, 1);
    vput(&mut p, member_off);
    vput(&mut p, 4096);
    p.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    vput(&mut p, 1); // n_free
    vput(&mut p, member_off + 1000);
    vput(&mut p, 100);
    vput(&mut p, 1); // freed_seq
    assert!(build("free-overlap", &p, 1, 1).contains("overlapping"));

    // A member extent reaching past the committed tail.
    let mut p = Vec::new();
    vput(&mut p, 1);
    vput(&mut p, 4);
    p.extend_from_slice(b"past");
    vput(&mut p, 1);
    vput(&mut p, member_off);
    vput(&mut p, 1 << 30);
    p.extend_from_slice(&0u32.to_le_bytes());
    vput(&mut p, 0);
    assert!(build("past-tail", &p, 1, 1).contains("committed region"));

    // An empty extent, which addresses nothing and may not be encoded.
    let mut p = Vec::new();
    vput(&mut p, 1);
    vput(&mut p, 5);
    p.extend_from_slice(b"empty");
    vput(&mut p, 1);
    vput(&mut p, member_off);
    vput(&mut p, 0);
    p.extend_from_slice(&0u32.to_le_bytes());
    vput(&mut p, 0);
    assert!(build("empty-extent", &p, 1, 1).contains("empty extent"));

    // A free extent claiming it was freed by a commit that has not happened.
    let mut p = Vec::new();
    vput(&mut p, 1);
    vput(&mut p, 5);
    p.extend_from_slice(b"whole");
    vput(&mut p, 1);
    vput(&mut p, member_off);
    vput(&mut p, 2048);
    p.extend_from_slice(&crc32fast::hash(&body[..2048]).to_le_bytes());
    vput(&mut p, 1);
    vput(&mut p, member_off + 2048);
    vput(&mut p, 100);
    vput(&mut p, 99); // freed_seq far beyond seq 1
    assert!(build("future-free", &p, 1, 1).contains("has not happened"));

    std::fs::remove_dir_all(&root).ok();
}

// ── reclaim's publication protocol: anchor, candidate, locked handoff, recovery ──────────────

/// Ten sessions of waste, closed cleanly: the input every reclaim test below starts from.
fn wasteful_store(ct: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut want = Vec::new();
    for round in 0..10 {
        let mut s = Store::open_file(ct, cfg()).unwrap();
        let id = format!("r:{round:02}");
        let body = noise(round, 1200);
        s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
        want.push((id, body));
        s.sync().unwrap();
        s.flush().unwrap();
        s.close().unwrap();
    }
    want
}

fn assert_serves(ct: &std::path::Path, want: &[(String, Vec<u8>)], what: &str) {
    Container::open(ct)
        .unwrap_or_else(|e| panic!("{what}: open: {e:#}"))
        .verify()
        .unwrap_or_else(|e| panic!("{what}: verify: {e:#}"));
    let rs = turndb::store::open_read_container(ct, cfg())
        .unwrap_or_else(|e| panic!("{what}: read: {e:#}"));
    for (id, body) in want {
        assert_eq!(rs.reconstruct(id).unwrap().as_deref(), Some(body.as_slice()), "{what}: {id}");
    }
    assert_eq!(rs.ids().unwrap().len(), want.len(), "{what}: record count");
}

fn names(ct: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(ct.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

fn is_writer_locked(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<turndb::fold::WriterLocked>().is_some())
}

#[test]
fn reclaim_leaves_exactly_one_file_and_the_store_serves_everything() {
    use turndb::container::reclaim;
    let root = tmp("reclaim-clean");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("grows.turndb");
    let want = wasteful_store(&ct);
    let stats = reclaim(&ct).unwrap();
    assert!(stats.reclaimed > 0);
    assert_eq!(
        names(&ct),
        vec!["grows.turndb".to_string()],
        "no anchor, candidate or staging left"
    );
    assert_serves(&ct, &want, "after reclaim");
    // The writer lock was released at return: a writer opens now.
    Store::open_file(&ct, cfg()).expect("writer opens after reclaim returned").close().unwrap();
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_absent_store_beside_a_whole_anchor_is_recovered_by_a_writer_open_and_refused_by_a_reader() {
    let root = tmp("reclaim-anchor");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("s.turndb");
    let want = wasteful_store(&ct);
    // Manufacture the crash state the protocol admits: the anchor published, the name gone.
    let anchor = root.join("s.turndb.reclaimed");
    std::fs::rename(&ct, &anchor).unwrap();
    let err = turndb::store::open_read_container(&ct, cfg()).err().expect("a reader refuses");
    assert!(format!("{err:#}").contains("reclaim anchor"), "{err:#}");
    assert!(!ct.exists(), "a reader created nothing");
    let s = Store::open_file(&ct, cfg()).expect("a writer open recovers from the anchor");
    s.close().unwrap();
    assert_serves(&ct, &want, "recovered store");
    assert_eq!(
        names(&ct),
        vec!["s.turndb".to_string()],
        "anchor and candidate gone after recovery"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_corrupt_or_incomplete_anchor_is_refused_and_nothing_is_created() {
    for (tag, damage) in [("corrupt", 0u8), ("truncated", 1u8)] {
        let root = tmp(&format!("reclaim-anchor-{tag}"));
        std::fs::create_dir_all(&root).unwrap();
        let ct = root.join("s.turndb");
        let _ = wasteful_store(&ct);
        let anchor = root.join("s.turndb.reclaimed");
        std::fs::rename(&ct, &anchor).unwrap();
        let bytes = std::fs::read(&anchor).unwrap();
        let before = bytes.clone();
        if damage == 0 {
            // Inside a LIVE member: a flip in superseded space would validate legitimately.
            let (off, len) = {
                let c = Container::open(&anchor).unwrap();
                let part = c.names().find(|n| n.starts_with("part-")).unwrap().to_string();
                c.member_extents(&part).unwrap()[0]
            };
            let mut b = bytes;
            let at = (off + len / 2) as usize;
            b[at] ^= 0xff;
            std::fs::write(&anchor, &b).unwrap();
        } else {
            std::fs::write(&anchor, &bytes[..bytes.len() - 700]).unwrap();
        }
        let err = match Store::open_file(&ct, cfg()) {
            Ok(_) => panic!("{tag}: a damaged anchor must not be promoted"),
            Err(e) => e,
        };
        let text = format!("{err:#}");
        assert!(
            text.contains("does not validate whole") || text.contains("anchor"),
            "{tag}: {text}"
        );
        assert!(!ct.exists(), "{tag}: no store was created: {text}");
        assert!(
            !root.join("s.turndb.reclaim-candidate").exists(),
            "{tag}: no candidate was published"
        );
        let after = std::fs::read(&anchor).unwrap();
        assert!(after.len() <= before.len(), "{tag}: the anchor was left as found");
        std::fs::remove_dir_all(&root).ok();
    }
}

#[test]
fn a_stale_anchor_beside_a_present_store_never_overrides_it_and_is_removed() {
    let root = tmp("reclaim-stale");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("s.turndb");
    let want = wasteful_store(&ct);
    // A stale anchor with DIFFERENT content: one more session's worth, never published.
    let other = root.join("other.turndb");
    let mut want_other = wasteful_store(&other);
    want_other.push(("extra".into(), b"never the store's".to_vec()));
    {
        let mut s = Store::open_file(&other, cfg()).unwrap();
        s.put("extra", &[Span::Piece(b"never the store's")], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
        s.close().unwrap();
    }
    std::fs::rename(&other, root.join("s.turndb.reclaimed")).unwrap();
    std::fs::write(root.join("s.turndb.reclaim-candidate"), b"debris").unwrap();
    let s = Store::open_file(&ct, cfg()).expect("the present store is authority");
    assert!(s.reconstruct("extra").unwrap().is_none(), "the anchor's content never entered");
    s.close().unwrap();
    assert_serves(&ct, &want, "store after stale anchor");
    assert_eq!(names(&ct), vec!["s.turndb".to_string()], "stale reclaim material removed");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_absent_store_with_a_broken_candidate_beside_a_whole_anchor_recovers_from_the_anchor() {
    let root = tmp("reclaim-bad-candidate");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("s.turndb");
    let want = wasteful_store(&ct);
    std::fs::rename(&ct, root.join("s.turndb.reclaimed")).unwrap();
    std::fs::write(root.join("s.turndb.reclaim-candidate"), b"torn").unwrap();
    std::fs::write(root.join("s.turndb.reclaim-candidate.tmp"), b"torn too").unwrap();
    Store::open_file(&ct, cfg())
        .expect("recovery rebuilds the candidate from the anchor")
        .close()
        .unwrap();
    assert_serves(&ct, &want, "recovered past a broken candidate");
    assert_eq!(names(&ct), vec!["s.turndb".to_string()]);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn reclaim_material_without_an_anchor_and_without_a_store_is_never_built_over() {
    let root = tmp("reclaim-orphan");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("s.turndb");
    std::fs::write(root.join("s.turndb.reclaim-candidate"), b"something").unwrap();
    let err = match Store::open_file(&ct, cfg()) {
        Ok(_) => panic!("must not create a store over unexplained reclaim material"),
        Err(e) => e,
    };
    assert!(format!("{err:#}").contains("not creating a new store over"), "{err:#}");
    assert!(!ct.exists());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn two_recovery_contenders_converge_on_one_store() {
    let root = tmp("reclaim-contend");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("s.turndb");
    let want = wasteful_store(&ct);
    std::fs::rename(&ct, root.join("s.turndb.reclaimed")).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let ct = ct.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                match Store::open_file(&ct, cfg()) {
                    Ok(s) => {
                        s.close().unwrap();
                        Ok(())
                    }
                    Err(e) => Err(is_writer_locked(&e)),
                }
            })
        })
        .collect();
    let outcomes: Vec<Result<(), bool>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(outcomes.iter().any(|o| o.is_ok()), "one contender recovers: {outcomes:?}");
    assert!(
        outcomes.iter().all(|o| matches!(o, Ok(()) | Err(true))),
        "any loser sees WriterLocked: {outcomes:?}"
    );
    assert_serves(&ct, &want, "after contention");
    assert_eq!(names(&ct), vec!["s.turndb".to_string()]);
    std::fs::remove_dir_all(&root).ok();
}

/// A 0.1.x working session beside a store is refused by name by a writer open and by reclaim,
/// and never removed — it may hold acknowledged writes only that release can settle.
#[test]
fn a_legacy_working_directory_is_refused_by_open_and_reclaim_and_never_removed() {
    let root = tmp("legacy-hot");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("s.turndb");
    let _ = wasteful_store(&ct);
    let hot = root.join("s.turndb-hot");
    std::fs::create_dir_all(&hot).unwrap();
    std::fs::write(hot.join("WAL"), b"acked").unwrap();
    let err = match Store::open_file(&ct, cfg()) {
        Ok(_) => panic!("a writer open must refuse"),
        Err(e) => e,
    };
    assert!(format!("{err:#}").contains("s.turndb-hot"), "{err:#}");
    let err = turndb::container::reclaim(&ct).unwrap_err();
    assert!(format!("{err:#}").contains("working directory"), "{err:#}");
    assert!(hot.join("WAL").exists(), "never removed");
    std::fs::remove_dir_all(&root).ok();
}

/// Beside an ABSENT store, a 0.1.x working directory refuses creation too: nothing is created,
/// nothing is removed, the path is named.
#[test]
fn a_legacy_working_directory_beside_an_absent_store_refuses_creation() {
    let root = tmp("legacy-hot-absent");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("s.turndb");
    let hot = root.join("s.turndb-hot");
    std::fs::create_dir_all(&hot).unwrap();
    std::fs::write(hot.join("WAL"), b"acked").unwrap();
    let err = match Store::open_file(&ct, cfg()) {
        Ok(_) => panic!("must not create a store beside a 0.1.x working directory"),
        Err(e) => e,
    };
    assert!(format!("{err:#}").contains("s.turndb-hot"), "{err:#}");
    assert!(!ct.exists(), "nothing created");
    assert!(hot.join("WAL").exists(), "nothing removed");
    std::fs::remove_dir_all(&root).ok();
}
