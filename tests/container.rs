//! The container gate: a store in one mutable file grows without invalidating what a reader
//! already resolved and survives a torn commit.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use turndb::container::{
    Container, ContainerReader, ALIGN, CONTAINER_DRAFT_EPOCH, MAGIC, REGION_START, SLOT_LEN,
};
use turndb::fold::FoldCfg;
use turndb::readat::ReadAt as _;
use turndb::store::{open_read_container, Span, Store};

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

fn rewrite_manifest_field(bytes: &[u8], before: &str, after: &str) -> Vec<u8> {
    let marker = b"\ncrc32=";
    let split = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("manifest checksum trailer");
    let payload = std::str::from_utf8(&bytes[..split]).unwrap();
    assert_eq!(payload.matches(before).count(), 1, "manifest field must be unique: {before}");
    let rewritten = payload.replacen(before, after, 1);
    let checksum = crc32fast::hash(rewritten.as_bytes());
    format!("{rewritten}\ncrc32={checksum:08x}").into_bytes()
}

fn rewrite_every_manifest(container: &mut Container, before: &str, after: &str) {
    let names: Vec<String> = container
        .names()
        .filter(|name| *name == "MANIFEST" || name.starts_with("MANIFEST."))
        .map(String::from)
        .collect();
    assert!(!names.is_empty(), "fixture must have manifest authority");
    for name in names {
        let bytes = container.read_file_bounded(&name, 1 << 20).unwrap();
        let rewritten = rewrite_manifest_field(&bytes, before, after);
        container.put_bytes(&name, &rewritten).unwrap();
    }
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
    let indexed_blocks: usize = segment_names
        .iter()
        .map(|segment| {
            let bytes =
                container.read_file_bounded(&segment.replace(".fold", ".dir"), 1 << 20).unwrap();
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize
        })
        .sum();
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
        4 + 2 * segment_names.len() + indexed_blocks + 11 * part_count + dictionary_count,
        "two slots, directory, manifest, one sidecar and segment header per segment, one frame-header proof per indexed block, one whole read per dictionary, and footer plus TOC plus checksum-authenticated structural metadata per part: {cold_reads:?}"
    );
    assert!(
        cold_reads.iter().all(|read| {
            payload_ranges.iter().all(|payload| {
                !overlaps(read, payload) || read.1 == turndb::fold::block::BLOCK_HDR_LEN
            })
        }),
        "a valid current container may prove frame headers but must not fetch fold payload merely to open: {cold_reads:?}"
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
        fallback_reads.iter().any(|read| {
            read.1 > turndb::fold::block::BLOCK_HDR_LEN
                && payload_ranges.iter().any(|payload| overlaps(read, payload))
        }),
        "without advisory open metadata the reader must derive it from the active payload"
    );
    assert_eq!(reader.reconstruct("trace/2/3").unwrap().unwrap().len(), 6 * 1024);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn committed_fold_bytes_after_the_last_complete_frame_are_refused_by_file_and_source_readers() {
    let root = tmp("committed-fold-trailing-byte");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("store.turndb");
    let mut store = Store::open_file(&path, cfg()).unwrap();
    store.put_body("record", &noise(41, 12 * 1024), vec![]).unwrap();
    store.sync().unwrap();
    store.flush().unwrap();
    store.close().unwrap();
    open_read_container(&path, cfg()).expect("nearest valid committed fold opens");

    let mut container = Container::open(&path).unwrap();
    let authority = container.read_file_bounded("MANIFEST", 1 << 20).unwrap();
    let split = authority.windows(7).position(|window| window == b"\ncrc32=").unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&authority[..split]).unwrap();
    let generation = manifest["fold_gen"].as_u64().unwrap();
    let segment = manifest["fold_seg"].as_u64().unwrap();
    let old_tail = manifest["fold_off"].as_u64().unwrap();
    let prefix = if generation == 0 { "fold".to_string() } else { format!("fold-{generation:04}") };
    let member = format!("{prefix}/seg-{segment:08}.fold");
    container
        .append_stream(&member, 1, |offset, into| {
            assert_eq!(offset, 0);
            into.fill(0x7f);
            Ok(())
        })
        .unwrap();
    rewrite_every_manifest(
        &mut container,
        &format!("\"fold_off\":{old_tail}"),
        &format!("\"fold_off\":{}", old_tail + 1),
    );
    container.commit().unwrap();
    drop(container);

    let file_error = open_read_container(&path, cfg()).err().expect("file reader must refuse");
    assert!(format!("{file_error:#}").contains("scans to"), "wrong refusal: {file_error:#}");
    let source = CountingSource::new(std::fs::read(&path).unwrap());
    let source_error = turndb::store::open_read_container_source(
        Arc::new(source),
        "memory://committed-fold-trailing-byte",
        cfg(),
        turndb::read_limits::ReadLimits::default(),
    )
    .err()
    .expect("source reader must refuse");
    assert!(format!("{source_error:#}").contains("scans to"), "wrong refusal: {source_error:#}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_manifest_cannot_declare_a_punched_block_that_does_not_exist() {
    let root = tmp("impossible-punched-block");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("store.turndb");
    let mut store = Store::open_file(&path, cfg()).unwrap();
    store.put_body("record", &noise(42, 12 * 1024), vec![]).unwrap();
    store.sync().unwrap();
    store.flush().unwrap();
    store.close().unwrap();
    open_read_container(&path, cfg()).expect("nearest valid manifest opens");

    let mut container = Container::open(&path).unwrap();
    rewrite_every_manifest(
        &mut container,
        "\"punched\":[]",
        "\"punched\":[[4294967295,4294967295]]",
    );
    container.commit().unwrap();
    drop(container);
    let malicious = std::fs::read(&path).unwrap();

    let read_error = open_read_container(&path, cfg()).err().expect("reader must refuse");
    assert!(
        format!("{read_error:#}").contains("names a block that does not exist"),
        "wrong refusal: {read_error:#}"
    );
    let writer_error = Store::open_file(&path, cfg()).err().expect("writer must refuse");
    assert!(
        format!("{writer_error:#}").contains("names a block that does not exist"),
        "wrong refusal: {writer_error:#}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), malicious, "refusal must not change the container");
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    assert!(!PathBuf::from(wal).exists(), "refusal must not create a WAL");
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
    let longest = "n".repeat(4096);
    c.put_bytes(&longest, b"edge").unwrap();
    let too_long = "n".repeat(4097);
    assert!(c.put_bytes(&too_long, b"x").is_err(), "the format's name ceiling is exact");
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
fn oversized_stream_ranges_refuse_before_mutating_the_container() {
    let root = tmp("oversized-stream");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("oversized-stream.turndb");
    let mut container = Container::create(&ct).unwrap();

    let before = std::fs::read(&ct).unwrap();
    let called = std::cell::Cell::new(false);
    assert!(container
        .put_stream("impossible", u64::MAX, |_, _| {
            called.set(true);
            Ok(())
        })
        .is_err());
    assert!(!called.get(), "range admission must precede the stream callback");
    assert_eq!(std::fs::read(&ct).unwrap(), before, "a refused range must write no bytes");
    assert!(!container.contains("impossible"));

    container.put_bytes("member", b"x").unwrap();
    let before = std::fs::read(&ct).unwrap();
    called.set(false);
    assert!(container
        .append_stream("member", u64::MAX, |_, _| {
            called.set(true);
            Ok(())
        })
        .is_err());
    assert!(!called.get(), "append range admission must precede the stream callback");
    assert_eq!(std::fs::read(&ct).unwrap(), before, "a refused append must write no bytes");
    assert_eq!(container.read_file_bounded("member", 8).unwrap(), b"x");

    std::fs::remove_dir_all(root).ok();
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
fn a_container_refuses_every_other_draft_epoch() {
    let root = tmp("version");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("version.turndb");

    let mut c = Container::create(&ct).unwrap();
    c.put_bytes("member", b"payload").unwrap();
    c.commit().unwrap();
    drop(c);

    // A superblock from another draft epoch must be refused, not misparsed — and because the
    // epoch byte is inside the checksummed prefix, forging one requires re-checksumming,
    // which is exactly what a future writer would do.
    let mut bytes = std::fs::read(&ct).unwrap();
    let live = newest_slot(&bytes);
    bytes[live + 49] = CONTAINER_DRAFT_EPOCH + 1;
    let digest = blake3::hash(&bytes[live..live + 52]);
    bytes[live + 52..live + 56].copy_from_slice(&digest.as_bytes()[0..4]);
    std::fs::write(&ct, &bytes).unwrap();

    // The older slot is still perfectly readable, and that is exactly why this must refuse: a
    // checksum-valid superblock from a newer writer is an authentic claim, so falling back to the
    // previous commit would serve a stale state while reporting success.
    let err = match Container::open(&ct) {
        Ok(_) => {
            panic!("a container from another draft epoch must reject rather than misread")
        }
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("draft epoch"), "the refusal must name the epoch lever, got: {err}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_authentic_malformed_older_slot_refuses_the_whole_container() {
    let root = tmp("malformed-older-slot");
    std::fs::create_dir_all(&root).unwrap();
    let ct = root.join("malformed-older-slot.turndb");

    let mut container = Container::create(&ct).unwrap();
    container.put_bytes("first", b"one").unwrap();
    container.commit().unwrap();
    container.put_bytes("second", b"two").unwrap();
    container.commit().unwrap();
    drop(container);

    let mut bytes = std::fs::read(&ct).unwrap();
    let newest = newest_slot(&bytes);
    let older = if newest == 0 { SLOT_LEN as usize } else { 0 };
    assert_eq!(u64::from_le_bytes(bytes[older + 8..older + 16].try_into().unwrap()), 1);
    bytes[older + 48] = 2; // authentic, but no such directory codec exists
    let digest = blake3::hash(&bytes[older..older + 52]);
    bytes[older + 52..older + 56].copy_from_slice(&digest.as_bytes()[..4]);
    std::fs::write(&ct, bytes).unwrap();

    let error = match Container::open(&ct) {
        Ok(_) => panic!("a malformed authentic slot must refuse the container"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("unknown directory codec"),
        "a malformed authentic claim must not be hidden by the newer valid slot: {error}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_container_from_the_discarded_format_is_refused_without_mutation() {
    let root = tmp("discarded-format");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("old.turndb");
    let mut container = Container::create(&path).unwrap();
    container.put_bytes("member", b"payload").unwrap();
    container.commit().unwrap();
    drop(container);

    let mut bytes = std::fs::read(&path).unwrap();
    let slot_at = newest_slot(&bytes);
    assert_eq!(bytes[slot_at..slot_at + 8], *MAGIC);
    bytes[slot_at..slot_at + 8].copy_from_slice(b"TURNCTNR");
    let digest = blake3::hash(&bytes[slot_at..slot_at + 52]);
    bytes[slot_at + 52..slot_at + 56].copy_from_slice(&digest.as_bytes()[..4]);
    std::fs::write(&path, &bytes).unwrap();

    assert!(
        Container::open(&path).is_err(),
        "an authentic discarded identity must refuse the whole open, not fall back to an older slot"
    );
    assert!(Store::open_file(&path, cfg()).is_err(), "a writer must not upgrade old bytes");
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "refusal must not rewrite the artifact");
    let mut wal = path.clone().into_os_string();
    wal.push("-wal");
    assert!(
        !std::path::PathBuf::from(wal).exists(),
        "refusing old bytes must not create writer state"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn arbitrary_short_final_name_artifacts_fail_closed_without_mutation() {
    let root = tmp("short-unknown");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("unknown.turndb");
    let bytes = vec![0u8; 100];
    std::fs::write(&path, &bytes).unwrap();

    assert!(Store::open_file(&path, cfg()).is_err(), "unknown short bytes must fail closed");
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "refusal must not rewrite unknown bytes");
    let mut wal = path.clone().into_os_string();
    wal.push("-wal");
    assert!(!std::path::PathBuf::from(wal).exists(), "refusal must not create writer state");

    let empty = root.join("empty-unknown.turndb");
    std::fs::write(&empty, []).unwrap();
    assert!(Store::open_file(&empty, cfg()).is_err(), "empty bytes carry no current identity");
    assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0, "refusal rewrote an empty artifact");
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

    // Closed but not settled: sync() then a plain drop leaves the acknowledged record as WAL replay
    // input, and rewriting under it would strand what only a writer close can retire.
    drop(s);
    let wal = {
        let mut p = ct.clone().into_os_string();
        p.push("-wal");
        PathBuf::from(p)
    };
    assert!(std::fs::metadata(&wal).unwrap().len() > 0, "the fixture must leave WAL replay input");
    let err = match reclaim(&ct) {
        Ok(stats) => panic!("reclaim must refuse WAL replay input, got {stats:?}"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("WAL replay input"), "the refusal must name why, got: {err}");

    // Publish and retire the replay input; reclaim then proceeds.
    let mut s = Store::open_file(&ct, cfg()).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();
    reclaim(&ct).expect("once the writer is gone and WAL input is retired, reclaim proceeds");
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
        slot[49] = CONTAINER_DRAFT_EPOCH;
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
    for (name, off, len) in [("first", member_off, 4096u64), ("second", member_off, 200u64)] {
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
    vput(&mut p, member_off);
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
    assert!(build("past-tail", &p, 1, 1).contains("member region"));

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

    // Wire order is part of the directory's canonical identity; a map must not sort hostile bytes
    // into legitimacy after the fact.
    let mut p = Vec::new();
    vput(&mut p, 2);
    for (name, off, len) in [("z", member_off, 100u64), ("a", member_off + 100, 100u64)] {
        vput(&mut p, name.len() as u64);
        p.extend_from_slice(name.as_bytes());
        vput(&mut p, 1);
        vput(&mut p, off);
        vput(&mut p, len);
        p.extend_from_slice(
            &crc32fast::hash(&body[(off - member_off) as usize..][..len as usize]).to_le_bytes(),
        );
    }
    vput(&mut p, 0);
    assert!(build("name-order", &p, 2, 1).contains("wire order"));

    let mut trailing = Vec::new();
    vput(&mut trailing, 0); // no members
    vput(&mut trailing, 0); // no free extents
    trailing.push(99);
    assert!(build("directory-trailing", &trailing, 0, 1).contains("trailing"));

    // Names are physical paths, not strings to normalize after parsing.
    let mut p = Vec::new();
    vput(&mut p, 1);
    vput(&mut p, 4);
    p.extend_from_slice(b"a//b");
    vput(&mut p, 1);
    vput(&mut p, member_off);
    vput(&mut p, 100);
    p.extend_from_slice(&crc32fast::hash(&body[..100]).to_le_bytes());
    vput(&mut p, 0);
    assert!(build("noncanonical-name", &p, 1, 1).contains("canonical"));

    // Sequence zero has one exact representation: both birth slots and no encoded directory.
    let mut p = Vec::new();
    vput(&mut p, 0);
    vput(&mut p, 0);
    assert!(build("sequence-zero-directory", &p, 0, 0).contains("sequence-zero"));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn superblock_reservations_and_the_empty_state_are_exact() {
    let root = tmp("strict-superblock");
    std::fs::create_dir_all(&root).unwrap();

    let reserved = root.join("reserved-tail.turndb");
    let mut container = Container::create(&reserved).unwrap();
    container.put_bytes("member", b"current").unwrap();
    container.commit().unwrap();
    drop(container);
    let mut bytes = std::fs::read(&reserved).unwrap();
    bytes[SLOT_LEN as usize + 56] = 1; // outside the checksum, but still defined as zero
    std::fs::write(&reserved, bytes).unwrap();
    assert!(Container::open(&reserved).is_err());

    let noncanonical = root.join("noncanonical-empty.turndb");
    Container::create(&noncanonical).unwrap();
    let mut bytes = std::fs::read(&noncanonical).unwrap();
    bytes[16..24].copy_from_slice(&(REGION_START + 1).to_le_bytes());
    let digest = blake3::hash(&bytes[0..52]);
    bytes[52..56].copy_from_slice(&digest.as_bytes()[0..4]);
    std::fs::write(&noncanonical, bytes).unwrap();
    assert!(Container::open(&noncanonical).is_err());

    // Two individually authentic slots cannot make contradictory claims at the same sequence.
    let contradictory = root.join("contradictory-slots.turndb");
    let mut container = Container::create(&contradictory).unwrap();
    container.put_bytes("member", b"current").unwrap();
    container.commit().unwrap();
    drop(container);
    let mut bytes = std::fs::read(&contradictory).unwrap();
    let second_slot = bytes[SLOT_LEN as usize..2 * SLOT_LEN as usize].to_vec();
    bytes[0..SLOT_LEN as usize].copy_from_slice(&second_slot);
    let altered_tail = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) + 1;
    bytes[40..48].copy_from_slice(&altered_tail.to_le_bytes());
    let digest = blake3::hash(&bytes[0..52]);
    bytes[52..56].copy_from_slice(&digest.as_bytes()[0..4]);
    std::fs::write(&contradictory, bytes).unwrap();
    assert!(Container::open(&contradictory).is_err());

    std::fs::remove_dir_all(root).ok();
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
fn reclaim_refuses_corrupt_source_members_instead_of_normalizing_them() {
    use std::io::{Seek, SeekFrom, Write};

    let root = tmp("reclaim-refuses-corrupt-source");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("source.turndb");
    let mut container = Container::create(&path).unwrap();
    container.put_bytes("member", &noise(50, 8 * 1024)).unwrap();
    container.commit().unwrap();
    container.put_bytes("member", &noise(51, 8 * 1024)).unwrap();
    container.commit().unwrap();
    let (offset, length) = container.member_extents("member").unwrap()[0];
    assert!(container.free_bytes() > 0, "fixture must make reclaim do work");
    drop(container);

    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(offset + length / 2)).unwrap();
    file.write_all(&[0x7f]).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let corrupt = std::fs::read(&path).unwrap();

    let error = turndb::container::reclaim(&path).unwrap_err();
    assert!(format!("{error:#}").contains("not a valid current-format store"), "{error:#}");
    assert_eq!(std::fs::read(&path).unwrap(), corrupt, "reclaim rewrote corrupt input");
    assert!(!root.join("source.turndb.reclaiming").exists());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn reclaim_refuses_a_valid_outer_container_that_is_not_a_current_store() {
    let root = tmp("reclaim-refuses-non-store");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("source.turndb");
    let mut container = Container::create(&path).unwrap();
    container.put_bytes("unknown-member", &noise(60, 8 * 1024)).unwrap();
    container.commit().unwrap();
    container.put_bytes("unknown-member", &noise(61, 8 * 1024)).unwrap();
    container.commit().unwrap();
    assert!(container.free_bytes() > 0, "fixture must give reclaim work to do");
    assert!(container.verify().is_ok(), "outer container bytes are internally valid");
    drop(container);
    let before = std::fs::read(&path).unwrap();

    let error = turndb::container::reclaim(&path).unwrap_err();
    assert!(
        format!("{error:#}").contains("not a valid current-format store"),
        "the refusal must come from full store validation: {error:#}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "reclaim mutated non-store input");
    assert!(!root.join("source.turndb.reclaiming").exists());
    std::fs::remove_dir_all(root).ok();
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
        let live_extent = if damage == 0 {
            let c = Container::open(&ct).unwrap();
            let part = c.names().find(|n| n.starts_with("part-")).unwrap().to_string();
            Some(c.member_extents(&part).unwrap()[0])
        } else {
            None
        };
        let anchor = root.join("s.turndb.reclaimed");
        std::fs::rename(&ct, &anchor).unwrap();
        let bytes = std::fs::read(&anchor).unwrap();
        let before = bytes.clone();
        if damage == 0 {
            // Inside a LIVE member: a flip in superseded space would validate legitimately.
            let (off, len) = live_extent.expect("corrupt case records a live extent");
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
