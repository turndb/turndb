//! Verification over a positioned source, in declared units: the same evidence as the
//! writer-side verification, produced in windows a range-fetching host can afford, each window
//! reading nothing it did not declare.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use turndb::container::Container;
use turndb::control::OperationControl;
use turndb::fold::FoldCfg;
use turndb::read_limits::ReadLimits;
use turndb::readat::ReadAt;
use turndb::store::{verify_source, SourceVerifier, Span, Store, VerifyUnit};
use turndb::types::AttrValue;

/// A memory source that records every read it serves, so a test can hold a unit to its footprint.
#[derive(Clone)]
struct RecordingSource {
    bytes: Arc<Vec<u8>>,
    reads: Arc<Mutex<Vec<(u64, u64)>>>,
}

impl RecordingSource {
    fn new(bytes: Vec<u8>) -> RecordingSource {
        RecordingSource { bytes: Arc::new(bytes), reads: Arc::new(Mutex::new(Vec::new())) }
    }

    fn take(&self) -> Vec<(u64, u64)> {
        std::mem::take(&mut *self.reads.lock().unwrap())
    }
}

impl ReadAt for RecordingSource {
    fn read_exact_at(&self, into: &mut [u8], offset: u64) -> io::Result<()> {
        let at = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        let bytes = self
            .bytes
            .get(at..at.saturating_add(into.len()))
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past the source"))?;
        into.copy_from_slice(bytes);
        self.reads.lock().unwrap().push((offset, into.len() as u64));
        Ok(())
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }
}

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir =
        std::env::temp_dir().join(format!("turndb-verify-source-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cfg() -> FoldCfg {
    // Small blocks and small segments: many frames, several segments, so windows and frame
    // boundaries are exercised rather than one block sitting in one segment.
    FoldCfg { block_target: 4 * 1024, seg_max: 16 * 1024, level: 3, ..Default::default() }
}

const M1: &[u8] =
    b"{\"role\":\"user\",\"content\":\"the first message, long enough to be worth folding\"}";
const M2: &[u8] =
    b"{\"role\":\"assistant\",\"content\":\"the second message, also reasonably long\"}";

fn put(s: &mut Store, id: &str, extra: &[u8]) {
    let spans = vec![
        Span::Lit(b"["),
        Span::Piece(M1),
        Span::Lit(b","),
        Span::Piece(M2),
        Span::Lit(b","),
        Span::Piece(extra),
        Span::Lit(b"]"),
    ];
    let attrs = vec![
        ("model".into(), AttrValue::Str("claude".into())),
        ("n".into(), AttrValue::Int(id.len() as i64)),
        ("ok".into(), AttrValue::Bool(true)),
    ];
    s.put(id, &spans, attrs).unwrap();
}

/// Several publications so the retained window is full, parts stack, rows are superseded and
/// tombstoned, and the fold rolls across segments.
fn fixture(dir: &Path) -> PathBuf {
    let path = dir.join("store.turndb");
    let mut s = Store::open_file(&path, cfg()).unwrap();
    // Incompressible per-record bodies, so the fold grows by real bytes and rolls segments.
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for flush in 0..6u32 {
        for i in 0..40u32 {
            let body: Vec<u8> = (0..600)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect();
            put(&mut s, &format!("rec:{:04}", i + flush * 25), &body);
        }
        if flush == 3 {
            s.delete("rec:0010").unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    s.close().unwrap();
    path
}

fn covered(read: (u64, u64), ranges: &[(u64, u64)]) -> bool {
    let (off, len) = read;
    if len == 0 {
        return true;
    }
    // Every byte of the read lies inside some declared range; ranges may abut, so walk.
    let mut at = off;
    let end = off + len;
    while at < end {
        match ranges.iter().find(|(start, run)| *start <= at && at < start + run) {
            Some((start, run)) => at = start + run,
            None => return false,
        }
    }
    true
}

fn flip_byte(path: &Path, at: u64) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(at)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    b[0] ^= 0xff;
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&b).unwrap();
    f.sync_all().unwrap();
}

fn member_start(path: &Path, name: &str) -> u64 {
    let c = Container::open(path).unwrap();
    c.member_extents(name).unwrap()[0].0
}

fn first_failing_unit(path: &Path) -> (VerifyUnit, String) {
    let bytes = std::fs::read(path).unwrap();
    let control = OperationControl::default();
    let mut v = SourceVerifier::open(
        Arc::new(RecordingSource::new(bytes)),
        "damaged",
        cfg(),
        ReadLimits::default(),
        &control,
    )
    .unwrap();
    for i in 0..v.units().len() {
        if let Err(error) = v.run(i, &control) {
            return (v.units()[i].clone(), format!("{error:#}"));
        }
    }
    panic!("every unit passed over damaged bytes");
}

#[test]
fn a_source_verification_composes_to_the_writer_side_result() {
    let dir = tmp("parity");
    let path = fixture(&dir);
    let want = Store::open_file(&path, cfg()).unwrap().verify().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let got = verify_source(
        Arc::new(RecordingSource::new(bytes)),
        "parity",
        cfg(),
        ReadLimits::default(),
        &OperationControl::default(),
    )
    .unwrap();

    assert!(want.parts >= 2, "the fixture must stack parts, got {}", want.parts);
    assert!(want.fold.segments >= 4, "the fixture must roll segments, got {}", want.fold.segments);
    assert_eq!(want.chain.retained_manifests, turndb::store::MANIFEST_RETAIN);
    assert_eq!(got.store.chain.retained_manifests, want.chain.retained_manifests);
    assert_eq!(got.store.chain.links, want.chain.links);
    assert_eq!(got.store.chain.part_digests, want.chain.part_digests);
    assert_eq!(got.store.fold.segments, want.fold.segments);
    assert_eq!(got.store.fold.blocks, want.fold.blocks);
    assert_eq!(got.store.fold.bytes, want.fold.bytes);
    assert_eq!(got.store.fold.trailing_uncommitted, want.fold.trailing_uncommitted);
    assert_eq!(got.store.parts, want.parts);
    assert_eq!(got.store.part_sections, want.part_sections);
    assert_eq!(got.store.records, want.records);
    assert_eq!(got.store.content_values, want.content_values);
    assert_eq!(got.store.content_bytes, want.content_bytes);
    assert_eq!(got.store.content_identities, want.content_identities);
    assert!(got.store.records > 0 && got.store.content_bytes > 0);
    let container = Container::open(&path).unwrap();
    assert_eq!(got.members, container.len());
    assert_eq!(got.member_bytes, container.member_bytes());
    assert_eq!(got.commit, 6);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn every_unit_reads_only_what_it_declared() {
    let dir = tmp("footprint");
    let path = fixture(&dir);
    let source = RecordingSource::new(std::fs::read(&path).unwrap());
    let control = OperationControl::default();
    let mut v = SourceVerifier::open(
        Arc::new(source.clone()),
        "footprint",
        cfg(),
        ReadLimits::default(),
        &control,
    )
    .unwrap();
    let metadata = source.take();
    assert!(!metadata.is_empty(), "open reads the superblocks at least");

    let mut kinds = HashSet::new();
    for i in 0..v.units().len() {
        let unit = v.units()[i].clone();
        kinds.insert(unit.kind());
        let footprint = v.footprint(i).unwrap();
        let mut declared = footprint.clone();
        declared.extend(metadata.iter().copied());
        for read in source.take() {
            assert!(
                covered(read, &declared),
                "computing the footprint of {unit:?} read {read:?} outside {footprint:?}"
            );
        }
        v.run(i, &control).unwrap();
        let reads = source.take();
        for read in &reads {
            assert!(
                covered(*read, &declared),
                "{unit:?} read {read:?} outside its footprint {footprint:?}"
            );
        }
        // A unit that reads nothing at all proves nothing; every kind but the chain walk (which
        // reads the manifests the open already parsed) and an empty member must touch bytes.
        let inert = matches!(unit, VerifyUnit::ManifestChain)
            || matches!(&unit, VerifyUnit::MemberChecksum { start, stop, .. } if start == stop);
        assert!(inert || !reads.is_empty(), "{unit:?} read nothing");
    }
    let expected: HashSet<&str> = [
        "memberChecksum",
        "manifestChain",
        "partDigest",
        "partSection",
        "partGrammar",
        "partPieces",
        "foldFrames",
        "content",
    ]
    .into_iter()
    .collect();
    assert_eq!(kinds, expected, "the plan must exercise every unit kind");
    v.report().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_partial_run_is_scoped_and_reruns_are_no_ops() {
    let dir = tmp("scoped");
    let path = fixture(&dir);
    let control = OperationControl::default();
    let mut v = SourceVerifier::open(
        Arc::new(RecordingSource::new(std::fs::read(&path).unwrap())),
        "scoped",
        cfg(),
        ReadLimits::default(),
        &control,
    )
    .unwrap();
    let total = v.units().len();
    assert!(total > 8);
    let refused = format!("{:#}", v.report().unwrap_err());
    assert!(refused.contains("scoped to 0 of"), "{refused}");
    v.run(0, &control).unwrap();
    v.run(0, &control).unwrap();
    assert_eq!(v.progress(), (1, total));
    let refused = format!("{:#}", v.report().unwrap_err());
    assert!(refused.contains(&format!("scoped to 1 of {total}")), "{refused}");
    for i in 1..total {
        v.run(i, &control).unwrap();
    }
    assert_eq!(v.progress(), (total, total));
    v.report().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn windows_over_a_long_member_run_in_order_and_fail_alone() {
    let dir = tmp("windows");
    let path = dir.join("store.turndb");
    let big = 9 << 20;
    {
        let mut s = Store::open_file(
            &path,
            FoldCfg { level: 1, block_target: 1 << 20, ..FoldCfg::default() },
        )
        .unwrap();
        // Incompressible bytes so the fold segment exceeds one window.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let body: Vec<u8> = (0..big)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        // Nine one-mebibyte pieces rather than one: a piece never splits across blocks, so one
        // piece would be one frame, and the test needs frame boundaries past the first window.
        for (i, piece) in body.chunks(1 << 20).enumerate() {
            s.put(&format!("big:{i}"), &[Span::Piece(piece)], vec![]).unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
        s.close().unwrap();
    }
    let control = OperationControl::default();
    let bytes = std::fs::read(&path).unwrap();
    let mut v = SourceVerifier::open(
        Arc::new(RecordingSource::new(bytes.clone())),
        "windows",
        cfg(),
        ReadLimits::default(),
        &control,
    )
    .unwrap();
    let segment = "fold/seg-00000000.fold";
    let windows: Vec<usize> = v
        .units()
        .iter()
        .enumerate()
        .filter(|(_, unit)| matches!(unit, VerifyUnit::MemberChecksum { member, .. } if member == segment))
        .map(|(i, _)| i)
        .collect();
    assert!(windows.len() >= 2, "a 9 MiB segment spans at least two 8 MiB windows");
    let frames: Vec<usize> = v
        .units()
        .iter()
        .enumerate()
        .filter(|(_, unit)| matches!(unit, VerifyUnit::FoldFrames { seg: 0, .. }))
        .map(|(i, _)| i)
        .collect();
    assert!(frames.len() >= 2, "frame windows split at frame boundaries past 8 MiB");
    let refused = format!("{:#}", v.run(windows[1], &control).unwrap_err());
    assert!(refused.contains("requires the window ending there first"), "{refused}");
    for i in 0..v.units().len() {
        v.run(i, &control).unwrap();
    }
    let report = v.report().unwrap();
    assert!(report.store.content_bytes >= big as u64);

    // Damage inside the second window: the first window still passes, the second fails, and a
    // rerun of the second window after the failure is judged afresh rather than on stale state.
    let start = member_start(&path, segment);
    flip_byte(&path, start + (8 << 20) + 4096);
    let mut v = SourceVerifier::open(
        Arc::new(RecordingSource::new(std::fs::read(&path).unwrap())),
        "windows",
        cfg(),
        ReadLimits::default(),
        &control,
    )
    .unwrap();
    v.run(windows[0], &control).unwrap();
    let first = format!("{:#}", v.run(windows[1], &control).unwrap_err());
    assert!(first.contains("fails its checksum"), "{first}");
    let again = format!("{:#}", v.run(windows[1], &control).unwrap_err());
    assert_eq!(first, again, "a failed window leaves its hasher state untouched");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn damage_is_found_by_the_unit_that_owns_the_bytes() {
    let dir = tmp("damage");
    let path = fixture(&dir);
    let pristine = std::fs::read(&path).unwrap();
    let control = OperationControl::default();

    // Inside a sealed fold segment's first frame payload: the member checksum finds it first in
    // plan order, and the frame walk, the piece dictionary, and the content units each find it on
    // their own when run alone.
    let seg0 = "fold/seg-00000000.fold";
    flip_byte(&path, member_start(&path, seg0) + 48 + 16 + 5);
    let (unit, message) = first_failing_unit(&path);
    assert!(
        matches!(&unit, VerifyUnit::MemberChecksum { member, .. } if member == seg0),
        "{unit:?}: {message}"
    );
    let mut v = SourceVerifier::open(
        Arc::new(RecordingSource::new(std::fs::read(&path).unwrap())),
        "damaged",
        cfg(),
        ReadLimits::default(),
        &control,
    )
    .unwrap();
    let units = v.units().to_vec();
    let frames = units
        .iter()
        .position(|unit| matches!(unit, VerifyUnit::FoldFrames { seg: 0, .. }))
        .unwrap();
    let frames_error = format!("{:#}", v.run(frames, &control).unwrap_err());
    assert!(
        frames_error.contains("fold frame at byte 48 fails its checksum"),
        "the damaged frame is named: {frames_error}"
    );
    let pieces =
        units.iter().position(|unit| matches!(unit, VerifyUnit::PartPieces { .. })).unwrap();
    let pieces_error = format!("{:#}", v.run(pieces, &control).unwrap_err());
    assert!(
        pieces_error.contains("checksum mismatch") || pieces_error.contains("hash mismatch"),
        "the damaged block is named: {pieces_error}"
    );
    let content_failures = units
        .iter()
        .enumerate()
        .filter(|(_, unit)| matches!(unit, VerifyUnit::Content { .. }))
        .filter(|(i, _)| v.run(*i, &control).is_err())
        .count();
    assert!(content_failures > 0, "some content unit reconstructs through the damaged frame");
    std::fs::write(&path, &pristine).unwrap();

    // Inside a part: in plan order the member checksum finds it; alone, the part's digest and
    // one of its section checksums do.
    let part = Container::open(&path)
        .unwrap()
        .names()
        .find(|name| name.starts_with("part-"))
        .unwrap()
        .to_string();
    let part_len = Container::open(&path).unwrap().member_len(&part).unwrap();
    flip_byte(&path, member_start(&path, &part) + part_len / 3);
    let (unit, message) = first_failing_unit(&path);
    assert!(
        matches!(&unit, VerifyUnit::MemberChecksum { member, .. } if *member == part),
        "{unit:?}: {message}"
    );
    let mut v = SourceVerifier::open(
        Arc::new(RecordingSource::new(std::fs::read(&path).unwrap())),
        "damaged",
        cfg(),
        ReadLimits::default(),
        &control,
    )
    .unwrap();
    let units = v.units().to_vec();
    let digest = units
        .iter()
        .position(|unit| matches!(unit, VerifyUnit::PartDigest { member, .. } if *member == part))
        .unwrap();
    let digest_error = format!("{:#}", v.run(digest, &control).unwrap_err());
    assert!(digest_error.contains("drifted from the digest"), "{digest_error}");
    let section_failures = units
        .iter()
        .enumerate()
        .filter(
            |(_, unit)| matches!(unit, VerifyUnit::PartSection { member, .. } if *member == part),
        )
        .filter(|(i, _)| v.run(*i, &control).is_err())
        .count();
    assert_eq!(section_failures, 1, "exactly the section holding the flipped byte fails");
    std::fs::write(&path, &pristine).unwrap();

    // The manifest: the chain walk owns it.
    flip_byte(&path, member_start(&path, "MANIFEST") + 40);
    let bytes = std::fs::read(&path).unwrap();
    let error = format!(
        "{:#}",
        SourceVerifier::open(
            Arc::new(RecordingSource::new(bytes)),
            "damaged",
            cfg(),
            ReadLimits::default(),
            &control
        )
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_else(|| {
            let (unit, message) = first_failing_unit(&path);
            assert!(
                matches!(unit, VerifyUnit::MemberChecksum { .. } | VerifyUnit::ManifestChain),
                "{unit:?}"
            );
            message
        })
    );
    assert!(!error.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unflushed_store_verifies_as_the_canonical_origin() {
    let dir = tmp("origin");
    let path = dir.join("store.turndb");
    Store::open_file(&path, cfg()).unwrap().close().unwrap();
    let report = verify_source(
        Arc::new(RecordingSource::new(std::fs::read(&path).unwrap())),
        "origin",
        cfg(),
        ReadLimits::default(),
        &OperationControl::default(),
    )
    .unwrap();
    assert_eq!(report.members, 0);
    assert_eq!(report.commit, 0);
    assert_eq!(report.store.records, 0);
    assert_eq!(report.store.chain.retained_manifests, 0);
    std::fs::remove_dir_all(&dir).ok();
}
