//! FORMAT.md, executed.
//!
//! A byte-layout document is the one place this project writes mechanics down twice, and the usual
//! objection to that is exactly right: a second copy drifts, and a drifted format document is worse
//! than none, because it is trusted. This is the answer to that objection. Every offset, magic,
//! width and invariant FORMAT.md states is asserted here against bytes an actual store just wrote.
//!
//! So the document cannot quietly stop being true. If someone moves a field, this fails, and the
//! failure names the section of FORMAT.md that needs updating with it.
//!
//! These are deliberately RAW byte assertions — offsets and slices, not calls into the decoders. A
//! test that used `parse_hdr` to check `parse_hdr`'s layout would pass no matter what the layout
//! became; that is the whole failure mode being guarded against.

use std::path::PathBuf;
use turndb::fold::{Loc, FoldCfg};
use turndb::store::{Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-format-{tag}-{}-{n}", std::process::id()))
}

/// A store with one flushed part and real content in the fold.
fn built(tag: &str) -> PathBuf {
    let dir = tmp(tag);
    let mut s = Store::open(&dir, FoldCfg::default()).unwrap();
    for i in 0..30u32 {
        let body: Vec<u8> = (0..400u32)
            .flat_map(|j| blake3::hash(&(i * 1000 + j).to_le_bytes()).as_bytes()[..8].to_vec())
            .collect();
        s.put(&format!("f{i:03}"), &[Span::Piece(&body)], vec![
            ("kind".into(), AttrValue::Str("req".into())),
            ("n".into(), AttrValue::Int(i as i64)),
        ])
        .unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    dir
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn segment(dir: &std::path::Path) -> Vec<u8> {
    let mut p: Vec<PathBuf> = std::fs::read_dir(dir.join("fold")).unwrap().flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "fold").unwrap_or(false))
        .collect();
    p.sort();
    std::fs::read(&p[0]).unwrap()
}

fn part(dir: &std::path::Path) -> Vec<u8> {
    let p = std::fs::read_dir(dir).unwrap().flatten()
        .map(|e| e.path())
        .find(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .expect("a flushed part");
    std::fs::read(&p).unwrap()
}

// ---------------------------------------------------------------------------------------------
// FORMAT.md § The fold — Segment
// ---------------------------------------------------------------------------------------------

#[test]
fn segment_header_matches_the_document() {
    let dir = built("seg");
    let b = segment(&dir);
    assert!(b.len() >= 48, "a segment is at least its 48-byte header");

    assert_eq!(&b[0..8], b"TURNFOLD", "magic at offset 0, 8 bytes");
    assert_eq!(le32(&b, 8), 0, "seg number at 8, and the first segment is 0");
    assert_eq!(le32(&b, 12), 0, "flags at 12 MUST BE ZERO — the reject-forward lever");
    assert_eq!(&b[16..48], &[0u8; 32], "dict_id at 16..48, all-zero for no dictionary");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_files_are_named_and_ordered_numerically() {
    let dir = built("segname");
    let names: Vec<String> = std::fs::read_dir(dir.join("fold")).unwrap().flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".fold")).collect();
    assert!(names.contains(&"seg-00000000.fold".to_string()),
        "FORMAT.md documents seg-%08u.fold; found {names:?}");
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// FORMAT.md § The fold — Block frame
// ---------------------------------------------------------------------------------------------

#[test]
fn block_frame_matches_the_document() {
    let dir = built("frame");
    let b = segment(&dir);
    let f = 48usize; // the first frame begins immediately after the header

    assert_eq!(b[f], 0xA5, "tag at frame+0 is 0xA5");
    let codec = b[f + 1];
    assert!(codec <= 2, "codec at frame+1 is 0, 1 or 2; got {codec}");
    let raw = le32(&b, f + 2);
    let stored = le32(&b, f + 6);
    let block_id = le32(&b, f + 12);

    assert!(stored <= raw, "FORMAT.md invariant: stored <= raw ({stored} > {raw})");
    if codec == 0 {
        assert_eq!(raw, stored, "FORMAT.md invariant: codec 0 implies raw == stored");
    }
    assert_eq!(block_id, 0, "block_id at frame+12; the first block written is id 0");

    // frame length is 20 + stored, so the tail checksum sits exactly there
    let end = f + 16 + stored as usize;
    assert!(end + 4 <= b.len(), "the frame and its 4-byte xsum must fit the segment");

    // r16 at frame+10 is the first two bytes of BLAKE3 over the block's RAW bytes
    let payload = &b[f + 16..end];
    let decoded = if codec == 0 { payload.to_vec() } else { zstd::bulk::decompress(payload, raw as usize).unwrap() };
    assert_eq!(decoded.len(), raw as usize, "raw is the DECOMPRESSED size of the whole block");
    let h = blake3::hash(&decoded);
    assert_eq!(&b[f + 10..f + 12], &h.as_bytes()[0..2], "r16 at frame+10");

    // xsum is BLAKE3 over frame[0 .. 16+stored], truncated to 4
    let x = blake3::hash(&b[f..end]);
    assert_eq!(&b[end..end + 4], &x.as_bytes()[0..4], "xsum over frame[0..16+stored]");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn loc_is_twelve_bytes_laid_out_as_documented() {
    assert_eq!(Loc::WIDTH, 12, "FORMAT.md documents Loc as 12 bytes");
    let l = Loc { block_id: 0x11223344, in_off: 0x55667788, raw: 0x99AABBCC };
    let b = l.encode();
    assert_eq!(le32(&b, 0), 0x11223344, "block_id at 0");
    assert_eq!(le32(&b, 4), 0x55667788, "in_off at 4");
    assert_eq!(le32(&b, 8), 0x99AABBCC, "raw at 8");
}

// ---------------------------------------------------------------------------------------------
// FORMAT.md § Parts — Footer
// ---------------------------------------------------------------------------------------------

#[test]
fn part_footer_matches_the_document() {
    let dir = built("footer");
    let b = part(&dir);
    let n = b.len();
    let f = n - 56; // FOOTER_LEN, at EOF

    assert_eq!(&b[f..f + 8], b"TURNPART", "magic at footer+0");
    let toc_off = le64(&b, f + 8);
    let toc_stored = le32(&b, f + 16);
    let toc_raw = le32(&b, f + 20);
    assert_eq!(le32(&b, f + 24), 30, "n_records at footer+24");
    assert_eq!(le64(&b, f + 28), 1, "seq_lo at footer+28");
    assert_eq!(le64(&b, f + 36), 1, "seq_hi at footer+36");
    // NOTE: this fixture has seq_lo == seq_hi, so it cannot distinguish the two fields on its own.
    // `merged_part_footer_distinguishes_seq_lo_from_seq_hi` is what pins their order.
    assert!(b[f + 44] <= 2, "toc_codec at footer+44");
    assert_eq!(b[f + 45], turndb::part::PART_VERSION, "version at footer+45");
    assert_eq!(&b[f + 50..f + 52], &[0u8; 2], "footer+50..52 is reserved and zero");

    let x = blake3::hash(&b[f..f + 52]);
    assert_eq!(&b[f + 52..f + 56], &x.as_bytes()[0..4], "xsum over footer[0..52]");

    // the TOC lives where the footer says, and is the documented size
    assert!(toc_off as usize + toc_stored as usize <= f, "the TOC must precede the footer");
    assert!(toc_raw >= toc_stored, "a TOC never expands under compression");

    // toc_xsum at footer+46 closes the chain: the footer checksums itself, this checksums the TOC,
    // and the TOC carries a checksum for every section. Without it the section checksums lived in
    // bytes nothing verified.
    let toc_xsum = le32(&b, f + 46);
    let toc_bytes = &b[toc_off as usize..toc_off as usize + toc_stored as usize];
    assert_eq!(toc_xsum, crc32fast::hash(toc_bytes), "toc_xsum at footer+46 over the STORED TOC");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merged_part_footer_distinguishes_seq_lo_from_seq_hi() {
    // A conformance test that cannot detect a field SWAP is not conforming anything. Every other
    // fixture flushes once, so seq_lo == seq_hi and the two are indistinguishable in the bytes.
    // Merging three parts gives a footer whose range is genuinely [1,3].
    let dir = tmp("seqrange");
    let mut s = Store::open(&dir, FoldCfg::default()).unwrap();
    for i in 0..3u32 {
        s.put(&format!("m{i}"), &[Span::Lit(b"x")], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
    }
    s.merge_range(0, 3).unwrap().unwrap();

    let b = part(&dir);
    let f = b.len() - 56;
    assert_eq!(le64(&b, f + 28), 1, "seq_lo at footer+28 is the LOW end of the merged range");
    assert_eq!(le64(&b, f + 36), 3, "seq_hi at footer+36 is the HIGH end");
    assert_eq!(le32(&b, f + 24), 3, "n_records at footer+24");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_part_declares_the_sections_the_document_lists() {
    let dir = built("sections");
    let p = turndb::part::Part::open(
        &std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path())
            .find(|p| p.extension().map(|e| e == "part").unwrap_or(false)).unwrap(),
    ).unwrap();
    let names: Vec<String> = p.sections().into_iter().map(|(n, _, _, _)| n).collect();
    for required in [
        "ids", "ids.restart", "prog", "prog.off",
        "pdict.loc", "pdict.hash", "pdict.hsort", "pdict.bloom",
        "layout", "layout.off", "colmeta",
    ] {
        assert!(names.contains(&required.to_string()),
            "FORMAT.md lists section {required}, which this part does not have: {names:?}");
    }
    // and the optional ones are absent exactly when unused, never malformed
    assert!(!names.contains(&"tomb".to_string()), "this part deletes nothing, so `tomb` is absent");

    // pdict.loc and pdict.hash are parallel: 12 bytes and 32 bytes per piece
    let secs = p.sections();
    let loc = secs.iter().find(|(n, _, _, _)| n == "pdict.loc").unwrap();
    let hash = secs.iter().find(|(n, _, _, _)| n == "pdict.hash").unwrap();
    assert_eq!(loc.2 as usize / 12, hash.2 as usize / 32,
        "pdict.loc (12 B/piece) and pdict.hash (32 B/piece) must describe the same pieces");
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// FORMAT.md § The write-ahead log, § The manifest
// ---------------------------------------------------------------------------------------------

#[test]
fn wal_frame_matches_the_document() {
    let dir = tmp("wal");
    let mut s = Store::open(&dir, FoldCfg::default()).unwrap();
    s.put("only", &[Span::Lit(b"body")], vec![]).unwrap();
    s.sync().unwrap();
    let b = std::fs::read(dir.join("WAL")).unwrap();

    assert_eq!(b[0], 0x57, "tag at 0 is 0x57 for a record");
    assert_eq!(le64(&b, 1), 0, "seq at 1");
    let len = le32(&b, 9) as usize;
    assert_eq!(13 + len + 4, b.len(), "header 13 + payload + crc 4 is the whole frame");

    // the crc covers the HEADER as well as the payload
    let mut h = crc32fast::Hasher::new();
    h.update(&b[0..13]);
    h.update(&b[13..13 + len]);
    assert_eq!(le32(&b, 13 + len), h.finalize(), "crc32 over header AND payload");

    // a deletion is tagged differently and carries the id alone
    s.delete("only").unwrap();
    s.sync().unwrap();
    let b = std::fs::read(dir.join("WAL")).unwrap();
    let t = 13 + len + 4;
    assert_eq!(b[t], 0x58, "tag 0x58 for a tombstone");
    assert_eq!(&b[t + 13..t + 13 + 4], b"only", "a tombstone payload is the id alone");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn manifest_carries_the_documented_fields() {
    let dir = built("manifest");
    let raw = std::fs::read_to_string(dir.join("MANIFEST")).unwrap();
    // FORMAT.md: compact JSON on the first line, then a `crc32=XXXXXXXX` trailer line over the
    // JSON bytes. The trailer is what turns corruption-that-still-parses into a refusal.
    let (json, trailer) = raw.split_once('\n').expect("a committed manifest carries a checksum trailer");
    let hex = trailer.strip_prefix("crc32=").expect("trailer line is crc32=XXXXXXXX");
    let want = u32::from_str_radix(hex, 16).expect("trailer carries hex");
    assert_eq!(crc32fast::hash(json.as_bytes()), want, "trailer must checksum the JSON payload");
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    for field in ["parts", "fold_seg", "fold_off", "next_seq", "fold_gen"] {
        assert!(v.get(field).is_some(), "FORMAT.md documents manifest field {field}: {raw}");
    }
    let p = &v["parts"][0];
    for field in ["file", "seq_lo", "seq_hi", "records"] {
        assert!(p.get(field).is_some(), "FORMAT.md documents part entry field {field}");
    }
    assert_eq!(v["fold_gen"], 0, "a store that has never re-folded is generation 0");
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// FORMAT.md § Limits
// ---------------------------------------------------------------------------------------------

#[test]
fn the_documented_limits_refuse_rather_than_truncate() {
    // FORMAT.md states these are enforced, not assumed — a store that cannot be written is
    // recoverable, one that lies is not. Each of these is the cheap end of a limit whose expensive
    // end (a 4 GiB section, a 4 GiB piece) cannot be exercised in a test.
    let dir = tmp("limits");
    assert!(
        turndb::fold::Fold::open(&dir.join("a"), FoldCfg { block_target: 5 << 30, ..FoldCfg::default() }).is_err(),
        "a block_target that would overflow the u32 segment append point must refuse"
    );
    assert!(
        turndb::fold::Fold::open(&dir.join("b"), FoldCfg { level: 99, ..FoldCfg::default() }).is_err(),
        "a zstd level outside 1..=22 must refuse at open"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Malformed input. Every writer-side assertion above says what a good part looks like; these say
// what a reader must do about a bad one. A format spec without them describes only the happy path.
// ---------------------------------------------------------------------------------------------

/// Rewrite a part's footer with `f`, repairing the footer checksum so only the edited field is tested.
fn edit_footer(path: &std::path::Path, f: impl Fn(&mut [u8])) {
    let mut b = std::fs::read(path).unwrap();
    let n = b.len();
    let start = n - 56;
    f(&mut b[start..n - 4]);
    let x = blake3::hash(&b[start..n - 4]);
    b[n - 4..].copy_from_slice(&x.as_bytes()[0..4]);
    std::fs::write(path, &b).unwrap();
}

fn part_path(dir: &std::path::Path) -> PathBuf {
    std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path())
        .find(|p| p.extension().map(|e| e == "part").unwrap_or(false)).unwrap()
}

#[test]
fn a_corrupt_toc_is_refused_rather_than_followed() {
    let dir = built("badtoc");
    let p = part_path(&dir);
    // Flip a byte inside the TOC payload. The footer still verifies, so before toc_xsum existed this
    // was followed straight into arbitrary offsets and allocations.
    let b = std::fs::read(&p).unwrap();
    let toc_off = le64(&b, b.len() - 56 + 8) as usize;
    let mut c = b.clone();
    c[toc_off] ^= 0xFF;
    std::fs::write(&p, &c).unwrap();
    assert!(turndb::part::Part::open(&p).is_err(), "a corrupt TOC must be refused");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_toc_pointing_past_itself_is_refused() {
    let dir = built("tocpast");
    let p = part_path(&dir);
    edit_footer(&p, |f| {
        // claim the TOC lives far beyond the file
        f[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    });
    assert!(turndb::part::Part::open(&p).is_err(), "a TOC offset past the file must be refused");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_future_version_is_refused_before_anything_is_parsed() {
    let dir = built("future");
    let p = part_path(&dir);
    edit_footer(&p, |f| f[45] = 200);
    let e = match turndb::part::Part::open(&p) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a future format version must not open"),
    };
    assert!(e.contains("format version"), "expected a version refusal, got: {e}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_truncated_part_is_refused() {
    let dir = built("trunc");
    let p = part_path(&dir);
    let b = std::fs::read(&p).unwrap();
    for keep in [0usize, 8, 55, b.len() / 2] {
        std::fs::write(&p, &b[..keep.min(b.len())]).unwrap();
        assert!(turndb::part::Part::open(&p).is_err(),
            "a part truncated to {keep} bytes must be refused, not half-read");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_segment_with_unknown_flags_is_refused() {
    let dir = built("flags");
    let mut sp: Vec<PathBuf> = std::fs::read_dir(dir.join("fold")).unwrap().flatten()
        .map(|e| e.path()).filter(|p| p.extension().map(|e| e == "fold").unwrap_or(false)).collect();
    sp.sort();
    let mut b = std::fs::read(&sp[0]).unwrap();
    b[12..16].copy_from_slice(&1u32.to_le_bytes()); // set an unknown flag bit
    std::fs::write(&sp[0], &b).unwrap();

    let e = match Store::open_read(&dir, FoldCfg::default()) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("an unknown segment flag must not open"),
    };
    assert!(e.contains("flags"), "expected a flags refusal, got: {e}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_fold_with_a_missing_segment_is_refused_by_readers_too() {
    // The writer always refused a gap. A reader that did not would serve a fold with a hole in its
    // block space instead of refusing — the worse half of an asymmetry.
    let dir = tmp("gap");
    let mut s = Store::open(&dir, FoldCfg { seg_max: 1 << 17, block_target: 1 << 14, ..FoldCfg::default() }).unwrap();
    for i in 0..300u32 {
        let body: Vec<u8> = (0..64u32)
            .flat_map(|j| blake3::hash(&(i * 500 + j).to_le_bytes()).as_bytes().to_vec()).collect();
        s.put(&format!("s{i:04}"), &[Span::Piece(&body)], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let mut segs: Vec<PathBuf> = std::fs::read_dir(dir.join("fold")).unwrap().flatten()
        .map(|e| e.path()).filter(|p| p.extension().map(|e| e == "fold").unwrap_or(false)).collect();
    segs.sort();
    assert!(segs.len() >= 3, "the fixture must produce several segments; got {}", segs.len());
    std::fs::remove_file(&segs[0]).unwrap(); // punch a hole at seg 0

    assert!(Store::open_read(&dir, FoldCfg::default()).is_err(),
        "a reader must refuse a fold whose segments are not dense");
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Reject-forward levers. A reservation the reader ignores reserves nothing.
// ---------------------------------------------------------------------------------------------

#[test]
fn reserved_footer_bytes_are_enforced_not_merely_documented() {
    // These were declared reserved and never read. A panel set them, repaired the footer checksum,
    // and this reader accepted the part — so a future writer could have used them and every shipped
    // build would have misread the result. The fold got this right from the start (`flags` bails);
    // the part did not.
    let dir = built("reserved");
    let p = part_path(&dir);
    edit_footer(&p, |f| f[50..52].copy_from_slice(&[0xAB, 0xCD]));
    let e = match turndb::part::Part::open(&p) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("non-zero reserved footer bytes must be refused"),
    };
    assert!(e.contains("reserved"), "expected a reserved-bytes refusal, got: {e}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_body_op_escape_is_reserved_and_never_written() {
    // The op tag is one bit with both values taken, so a third op has nowhere to live. `tagged == 0`
    // is a zero-length literal: reachable, contributes nothing, and never emitted. Reserving it buys
    // an unbounded future op space for zero bytes — but only if today's reader refuses it, since
    // otherwise it would parse a future escape's payload as ops.
    let dir = tmp("escape");
    let mut s = Store::open(&dir, FoldCfg::default()).unwrap();
    // an empty literal is dropped rather than encoded, so the reserved codepoint never appears
    s.put("r", &[Span::Lit(b""), Span::Lit(b"real"), Span::Lit(b"")], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(s.reconstruct("r").unwrap().unwrap(), b"real".to_vec(),
        "dropping an empty literal must preserve the body exactly");
    drop(s);

    // and the encoded program contains no zero tag
    let p = turndb::part::Part::open(&part_path(&dir)).unwrap();
    let ops = p.body(0).unwrap();
    assert_eq!(ops.len(), 1, "the two empty literals are gone, the real one remains: {ops:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_wal_frame_from_a_newer_build_is_refused_not_silently_dropped() {
    // An unknown tag is ambiguous: a crash mid-append leaves garbage (the log ends), and a newer
    // writer's frame also lands here (refusing is the only safe reading). Treating both as "end of
    // log" meant a future frame type would silently discard every committed record after it. The crc
    // disambiguates — a torn tail does not checksum, a deliberate frame does.
    let dir = tmp("walfuture");
    let mut s = Store::open(&dir, FoldCfg::default()).unwrap();
    s.put("a", &[Span::Lit(b"first")], vec![]).unwrap();
    s.sync().unwrap();
    drop(s);

    // append a well-formed frame with an unknown tag
    let path = dir.join("WAL");
    let mut b = std::fs::read(&path).unwrap();
    let payload = b"a frame type this build does not know";
    let mut hdr = vec![0x5Fu8];
    hdr.extend_from_slice(&99u64.to_le_bytes());
    hdr.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    let mut h = crc32fast::Hasher::new();
    h.update(&hdr);
    h.update(payload);
    b.extend_from_slice(&hdr);
    b.extend_from_slice(payload);
    b.extend_from_slice(&h.finalize().to_le_bytes());
    std::fs::write(&path, &b).unwrap();

    let e = match Store::open(&dir, FoldCfg::default()) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a checksumming frame with an unknown tag must be refused"),
    };
    assert!(e.contains("tag"), "expected a frame-tag refusal, got: {e}");

    // ...while a genuinely TORN tail still just ends the log, as it must
    let mut torn = std::fs::read(&path).unwrap();
    let n = torn.len();
    torn[n - 1] ^= 0xFF; // break the crc
    std::fs::write(&path, &torn).unwrap();
    let s = Store::open(&dir, FoldCfg::default()).expect("a torn tail is the end of the log, not an error");
    assert_eq!(s.reconstruct("a").unwrap().unwrap(), b"first".to_vec());
    std::fs::remove_dir_all(&dir).ok();
}
