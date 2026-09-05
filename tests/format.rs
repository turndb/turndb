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

use std::path::{Path, PathBuf};
use turndb::fold::{FoldCfg, Loc};
use turndb::store::{Batch, ContentSpans, Span, Store};
use turndb::types::ContentHash;
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-format-{tag}-{}-{n}", std::process::id()))
}

fn wal_sidecar(store: &Path) -> PathBuf {
    let mut p = store.as_os_str().to_os_string();
    p.push("-wal");
    PathBuf::from(p)
}

/// A store with one flushed part and real content in the fold, laid out for raw parsing.
fn built(tag: &str) -> PathBuf {
    let dir = tmp(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(&store, FoldCfg::default()).unwrap();
    for i in 0..30u32 {
        let body: Vec<u8> = (0..400u32)
            .flat_map(|j| blake3::hash(&(i * 1000 + j).to_le_bytes()).as_bytes()[..8].to_vec())
            .collect();
        s.put(
            &format!("f{i:03}"),
            &[Span::Piece(&body)],
            vec![
                ("kind".into(), AttrValue::Str("req".into())),
                ("n".into(), AttrValue::Int(i as i64)),
            ],
        )
        .unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();
    materialize(&store, &dir);
    dir
}

/// Materialize container members beneath `dir` so the tests can inspect each physical artifact
/// directly. The hot WAL sidecar is copied beside those extracted members when present.
fn materialize(store: &Path, dir: &std::path::Path) {
    let c = turndb::container::Container::open(store).unwrap();
    for name in c.names().map(String::from).collect::<Vec<_>>() {
        let bytes = c.read_file_bounded(&name, 1 << 30).unwrap();
        let out = dir.join(&name);
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(&out, bytes).unwrap();
    }
    let mut wal = store.as_os_str().to_os_string();
    wal.push("-wal");
    if let Ok(bytes) = std::fs::read(&wal) {
        std::fs::write(dir.join("WAL"), bytes).unwrap();
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn segment(dir: &std::path::Path) -> Vec<u8> {
    let mut p: Vec<PathBuf> = std::fs::read_dir(dir.join("fold"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "fold").unwrap_or(false))
        .collect();
    p.sort();
    std::fs::read(&p[0]).unwrap()
}

fn part(dir: &std::path::Path) -> Vec<u8> {
    let p = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .expect("a flushed part");
    std::fs::read(&p).unwrap()
}

/// LEB128-style unsigned varint, decoded exactly as FORMAT.md states it — 7 bits per byte, low
/// byte first — so the tests below share none of the library's own parsing.
fn varint(b: &[u8], at: &mut usize) -> u64 {
    let mut out = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = b[*at];
        *at += 1;
        out |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return out;
        }
        shift += 7;
    }
}

fn decode_codec(codec: u8, stored: &[u8], raw: usize) -> Vec<u8> {
    match codec {
        0 => stored.to_vec(),
        1 => zstd::bulk::decompress(stored, raw).unwrap(),
        c => panic!("unexpected codec {c} in a dictionary-less part"),
    }
}

/// A section's raw bytes, located by walking the footer and TOC exactly as FORMAT.md documents
/// them — offsets and varints by hand, no turndb decoder involved.
fn raw_section(part: &[u8], want: &str) -> Vec<u8> {
    let f = part.len() - 56;
    assert_eq!(part[f + 45], 1, "part footer carries the one current draft epoch");
    let toc_off = le64(part, f + 8) as usize;
    let toc_stored = le32(part, f + 16) as usize;
    let toc_raw = le32(part, f + 20) as usize;
    let toc = decode_codec(part[f + 44], &part[toc_off..toc_off + toc_stored], toc_raw);
    let mut at = 0usize;
    let n = varint(&toc, &mut at) as usize;
    for _ in 0..n {
        let name_len = varint(&toc, &mut at) as usize;
        let name = std::str::from_utf8(&toc[at..at + name_len]).unwrap().to_string();
        at += name_len;
        let off = varint(&toc, &mut at) as usize;
        let stored = varint(&toc, &mut at) as usize;
        let raw = varint(&toc, &mut at) as usize;
        let codec = toc[at];
        at += 1;
        at += 4; // per-section checksum is mandatory in the current encoding
        if name == want {
            return decode_codec(codec, &part[off..off + stored], raw);
        }
    }
    panic!("part has no section {want}");
}

// ---------------------------------------------------------------------------------------------
// FORMAT.md § The fold — Segment
// ---------------------------------------------------------------------------------------------

#[test]
fn segment_header_matches_the_document() {
    let dir = built("seg");
    let b = segment(&dir);
    assert!(b.len() >= 48, "a segment is at least its 48-byte header");

    assert_eq!(&b[0..8], b"TDBFLD01", "magic at offset 0, 8 bytes");
    assert_eq!(le32(&b, 8), 0, "seg number at 8, and the first segment is 0");
    assert_eq!(le32(&b, 12), 0, "flags at 12 MUST BE ZERO — the reject-forward lever");
    assert_eq!(&b[16..48], &[0u8; 32], "dict_id at 16..48, all-zero for no dictionary");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_files_are_named_and_ordered_numerically() {
    let dir = built("segname");
    let names: Vec<String> = std::fs::read_dir(dir.join("fold"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".fold"))
        .collect();
    assert!(
        names.contains(&"seg-00000000.fold".to_string()),
        "FORMAT.md documents seg-%08u.fold; found {names:?}"
    );
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
    let decoded = if codec == 0 {
        payload.to_vec()
    } else {
        zstd::bulk::decompress(payload, raw as usize).unwrap()
    };
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

    assert_eq!(&b[f..f + 8], b"TDBPRT01", "magic at footer+0");
    let toc_off = le64(&b, f + 8);
    let toc_stored = le32(&b, f + 16);
    let toc_raw = le32(&b, f + 20);
    assert_eq!(le32(&b, f + 24), 30, "n_records at footer+24");
    assert_eq!(le64(&b, f + 28), 1, "seq_lo at footer+28");
    assert_eq!(le64(&b, f + 36), 1, "seq_hi at footer+36");
    // NOTE: this fixture has seq_lo == seq_hi, so it cannot distinguish the two fields on its own.
    // `merged_part_footer_distinguishes_seq_lo_from_seq_hi` is what pins their order.
    assert!(b[f + 44] <= 2, "toc_codec at footer+44");
    assert_eq!(b[f + 45], 1, "draft epoch at footer+45");
    assert_eq!(turndb::part::PART_DRAFT_EPOCH, 1, "and PART_DRAFT_EPOCH agrees with the document");
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
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(&store, FoldCfg::default()).unwrap();
    for i in 0..3u32 {
        s.put(&format!("m{i}"), &[Span::Lit(b"x")], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
    }
    s.merge_range(0, 3).unwrap().unwrap();
    s.close().unwrap();
    materialize(&store, &dir);

    // The three merged-away originals remain on disk until the sweep, so the directory holds four
    // `.part` files and `read_dir` order — filesystem-dependent — must not pick one. FORMAT.md
    // names a merge output by its sequence RANGE (`part-<lo>-<hi>.part`, two dashes); select it
    // by that documented name.
    let b = std::fs::read(
        std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with(".part") && n.matches('-').count() == 2)
                    .unwrap_or(false)
            })
            .expect("a merged part named by its sequence range"),
    )
    .unwrap();
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
        &std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().map(|e| e == "part").unwrap_or(false))
            .unwrap(),
    )
    .unwrap();
    let names: Vec<String> = p.sections().into_iter().map(|(n, _, _, _)| n).collect();
    for required in [
        "ids",
        "ids.restart",
        "cmeta",
        "con.prog.0",
        "con.off.0",
        "con.id.0",
        "pdict.loc",
        "pdict.hash",
        "pdict.hsort",
        "pdict.bloom",
        "layout",
        "layout.off",
        "colmeta",
    ] {
        assert!(
            names.contains(&required.to_string()),
            "FORMAT.md lists section {required}, which this part does not have: {names:?}"
        );
    }
    // and the optional ones are absent exactly when unused, never malformed
    assert!(!names.contains(&"tomb".to_string()), "this part deletes nothing, so `tomb` is absent");

    // pdict.loc and pdict.hash are parallel: 12 bytes and 32 bytes per piece
    let secs = p.sections();
    let loc = secs.iter().find(|(n, _, _, _)| n == "pdict.loc").unwrap();
    let hash = secs.iter().find(|(n, _, _, _)| n == "pdict.hash").unwrap();
    let identities = secs.iter().find(|(n, _, _, _)| n == "con.id.0").unwrap();
    assert_eq!(
        loc.2 as usize / 12,
        hash.2 as usize / 32,
        "pdict.loc (12 B/piece) and pdict.hash (32 B/piece) must describe the same pieces"
    );
    assert_eq!(identities.2, 30 * 32, "one fixed-width identity entry per content occurrence");
    let first_body: Vec<u8> =
        (0..400u32).flat_map(|j| blake3::hash(&j.to_le_bytes()).as_bytes()[..8].to_vec()).collect();
    assert_eq!(p.content_identity(0, "body").unwrap(), Some(ContentHash::of(&first_body)));
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// FORMAT.md § The write-ahead log, § The manifest
// ---------------------------------------------------------------------------------------------

#[test]
fn wal_frame_matches_the_document() {
    let dir = tmp("wal");
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(&store, FoldCfg::default()).unwrap();
    s.put("only", &[Span::Lit(b"body")], vec![]).unwrap();
    s.sync().unwrap();
    let b = std::fs::read(wal_sidecar(&store)).unwrap();

    assert_eq!(b[0], 0xD4, "tag at 0 is the current record tag");
    assert_eq!(le64(&b, 1), 1, "the initial part-sequence target at byte 1");
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
    let b = std::fs::read(wal_sidecar(&store)).unwrap();
    let t = 13 + len + 4;
    assert_eq!(b[t], 0xD1, "current tombstone tag");
    assert_eq!(&b[t + 13..t + 13 + 4], b"only", "a tombstone payload is the id alone");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn wal_batch_frames_carry_the_documented_tags() {
    let dir = tmp("walbatchtags");
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(&store, FoldCfg::default()).unwrap();
    s.put("seed", &[Span::Lit(b"z")], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap(); // truncates the WAL, so the batch frames start at offset 0

    let mut batch = Batch::new();
    batch.put("member", &[Span::Lit(b"m")], vec![]);
    batch.delete("seed");
    s.apply(batch).unwrap();
    s.sync().unwrap();
    drop(s);
    let b = std::fs::read(wal_sidecar(&store)).unwrap();

    assert_eq!(b[0], 0xD5, "current in-batch record tag");
    let len0 = le32(&b, 9) as usize;
    let t1 = 13 + len0 + 4;
    assert_eq!(b[t1], 0xD3, "current in-batch tombstone tag");
    let len1 = le32(&b, t1 + 9) as usize;
    assert_eq!(&b[t1 + 13..t1 + 13 + len1], b"seed", "a batch tombstone payload is the id alone");
    let t2 = t1 + 13 + len1 + 4;
    assert_eq!(b[t2], 0xD2, "current batch completion marker");
    assert_eq!(le32(&b, t2 + 9), 1, "the marker payload is one varint");
    assert_eq!(b[t2 + 13], 2, "committing exactly the two members before it");
    assert_eq!(b.len(), t2 + 13 + 1 + 4, "nothing follows the completion marker");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn wal_record_payload_matches_the_documented_layout() {
    // FORMAT.md § The write-ahead log — the current record payload, walked field by field at
    // hand-computed offsets. Every expected byte below derives from the documented layout, not
    // from calling the encoder: mandatory identity placement, the plain-u8 op tag, and the value
    // widths of attribute tags 4 (u64), 5 (binary), 6 (timestamp), 7 (explicit null).
    let dir = tmp("walpayload");
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(&store, FoldCfg::default()).unwrap();
    s.put(
        "rec",
        &[Span::Lit(b"xy")],
        vec![
            ("u".into(), AttrValue::UInt(u64::MAX)),
            ("b".into(), AttrValue::Bytes(vec![0x00, 0xFF])),
            ("t".into(), AttrValue::TimestampNs(-2)),
            ("n".into(), AttrValue::Null),
        ],
    )
    .unwrap();
    s.sync().unwrap();
    drop(s);
    let b = std::fs::read(wal_sidecar(&store)).unwrap();

    assert_eq!(b[0], 0xD4, "tag at 0 is the current record tag");
    let len = le32(&b, 9) as usize;
    assert_eq!(len, 80, "the documented layout of this record is exactly 80 bytes");
    assert_eq!(b.len(), 13 + 80 + 4, "one frame: header 13, payload, crc 4");
    let p = &b[13..13 + len];

    assert_eq!(p[0], 3, "varint id_len at payload+0");
    assert_eq!(&p[1..4], b"rec", "id bytes at payload+1");
    assert_eq!(p[4], 1, "varint n_contents at payload+4");
    assert_eq!(p[5], 4, "varint name_len at payload+5");
    assert_eq!(&p[6..10], b"body", "utf8 content name at payload+6");
    assert_eq!(
        &p[10..42],
        blake3::hash(b"xy").as_bytes(),
        "the mandatory 32-byte whole-value BLAKE3 immediately follows the name"
    );
    assert_eq!(p[42], 1, "varint n_ops at payload+42");
    assert_eq!(p[43], 0, "op 0 (literal) is a PLAIN u8 in the log, unlike a part's packed varint");
    assert_eq!(p[44], 2, "varint literal length");
    assert_eq!(&p[45..47], b"xy", "literal bytes inline");
    assert_eq!(p[47], 4, "varint n_attrs at payload+47");
    // tag 4 (u64): 8 bytes, full unsigned range
    assert_eq!(&p[48..51], &[1, b'u', 4], "key length, key, then type tag 4");
    assert_eq!(&p[51..59], &[0xFF; 8], "u64::MAX as 8 little-endian bytes");
    // tag 5 (binary): varint length then the bytes
    assert_eq!(&p[59..62], &[1, b'b', 5], "key length, key, then type tag 5");
    assert_eq!(&p[62..65], &[0x02, 0x00, 0xFF], "varint len 2 then the value bytes");
    // tag 6 (timestamp): 8 bytes of signed UTC Unix nanoseconds
    assert_eq!(&p[65..68], &[1, b't', 6], "key length, key, then type tag 6");
    assert_eq!(&p[68..76], &(-2i64).to_le_bytes(), "signed nanoseconds, 8 bytes little-endian");
    // tag 7 (explicit null): zero value bytes
    assert_eq!(&p[76..79], &[1, b'n', 7], "key length, key, then type tag 7 — and no value");
    assert_eq!(p[79], 0, "varint n_novel at payload+79: a literal introduces no pieces");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cmeta_bytes_match_the_documented_layout() {
    // FORMAT.md § Named content columns: varint n_content_columns, then per column a varint
    // name length, the UTF-8 name, a varint occurrence count, and a u8 rid_kind (0 dense).
    let dir = built("cmeta");
    let b = part(&dir);
    let cmeta = raw_section(&b, "cmeta");
    let mut expected = vec![0x01, 0x04]; // one column, 4-byte name
    expected.extend_from_slice(b"body");
    expected.push(30); // occurrences: every fixture row carries a body
    expected.push(0x00); // rid_kind 0: dense, so con.rid.0 is elided
    assert_eq!(cmeta, expected, "cmeta must be exactly the documented bytes");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn binary_attribute_dictionary_matches_the_documented_layout() {
    // FORMAT.md § col.dict.N and the type table: a binary (tag 5) column stores a byte-sorted,
    // distinct dictionary — varint count, then varint len + bytes per entry — and its value
    // column is u32 ordinals into that dictionary, one per occurrence in row order.
    let dir = tmp("bindict");
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(&store, FoldCfg::default()).unwrap();
    for (id, raw) in [("a", vec![0xBB]), ("b", vec![0xAA]), ("c", vec![0xBB])] {
        s.put(id, &[Span::Lit(b"x")], vec![("raw".into(), AttrValue::Bytes(raw))]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();
    materialize(&store, &dir);
    let b = part(&dir);

    let dict = raw_section(&b, "col.dict.0");
    assert_eq!(
        dict,
        vec![0x02, 0x01, 0xAA, 0x01, 0xBB],
        "two distinct entries, byte-sorted, duplicates collapsed"
    );
    let val = raw_section(&b, "col.val.0");
    let expected: Vec<u8> = [1u32, 0, 1].iter().flat_map(|v| v.to_le_bytes()).collect();
    assert_eq!(val, expected, "u32 ordinals into the sorted dictionary, one per occurrence");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn con_off_matches_the_documented_layout_for_a_multi_content_record() {
    // FORMAT.md § Named content columns: con.off.N holds occurrences + 1 little-endian u64
    // offsets into con.prog.N, cumulative from 0, and columns are ordered by UTF-8 name.
    let dir = tmp("conoff");
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(&store, FoldCfg::default()).unwrap();
    s.put_record(
        "m0",
        &[
            ContentSpans::new("alpha", vec![Span::Lit(b"one")]),
            ContentSpans::new("beta", vec![Span::Lit(b"four")]),
        ],
        vec![],
    )
    .unwrap();
    s.put_record(
        "m1",
        &[
            ContentSpans::new("alpha", vec![Span::Lit(b"seven77")]),
            ContentSpans::new("beta", vec![Span::Lit(b"xy")]),
        ],
        vec![],
    )
    .unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();
    materialize(&store, &dir);
    let b = part(&dir);

    // cmeta names the columns in UTF-8 order: alpha is column 0, beta is column 1.
    let cmeta = raw_section(&b, "cmeta");
    let mut expected_cmeta = vec![0x02, 0x05];
    expected_cmeta.extend_from_slice(b"alpha");
    expected_cmeta.extend_from_slice(&[0x02, 0x00, 0x04]);
    expected_cmeta.extend_from_slice(b"beta");
    expected_cmeta.extend_from_slice(&[0x02, 0x00]);
    assert_eq!(cmeta, expected_cmeta, "two dense columns, two occurrences each");

    // A part program for a literal of length L is: varint n_ops = 1, varint (L << 1) | 0, then
    // the bytes — so "one" costs 5 bytes and "seven77" costs 9, laid down in occurrence order.
    let prog0 = raw_section(&b, "con.prog.0");
    let mut expected_prog = vec![0x01, 0x06];
    expected_prog.extend_from_slice(b"one");
    expected_prog.extend_from_slice(&[0x01, 0x0E]);
    expected_prog.extend_from_slice(b"seven77");
    assert_eq!(prog0, expected_prog, "part programs pack (payload << 1) | op as one varint");

    let off0 = raw_section(&b, "con.off.0");
    let expected_off: Vec<u8> = [0u64, 5, 14].iter().flat_map(|v| v.to_le_bytes()).collect();
    assert_eq!(off0, expected_off, "occurrences + 1 u64 offsets, cumulative over column 0");

    let off1 = raw_section(&b, "con.off.1");
    let expected_off: Vec<u8> = [0u64, 6, 10].iter().flat_map(|v| v.to_le_bytes()).collect();
    assert_eq!(off1, expected_off, "beta's programs are 6 and 4 bytes");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn manifest_carries_the_documented_fields() {
    let dir = built("manifest");
    let raw = std::fs::read_to_string(dir.join("MANIFEST")).unwrap();
    // FORMAT.md: compact JSON on the first line, then a `crc32=XXXXXXXX` trailer line over the
    // JSON bytes. The trailer is what turns corruption-that-still-parses into a refusal.
    let (json, trailer) =
        raw.split_once('\n').expect("a committed manifest carries a checksum trailer");
    let hex = trailer.strip_prefix("crc32=").expect("trailer line is crc32=XXXXXXXX");
    let want = u32::from_str_radix(hex, 16).expect("trailer carries hex");
    assert_eq!(crc32fast::hash(json.as_bytes()), want, "trailer must checksum the JSON payload");
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    for field in ["parts", "fold_seg", "fold_off", "next_seq", "fold_gen", "commit"] {
        assert!(v.get(field).is_some(), "FORMAT.md documents manifest field {field}: {raw}");
    }
    let p = &v["parts"][0];
    for field in ["member", "seq_lo", "seq_hi", "records", "b3"] {
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
        turndb::fold::Fold::open(
            &dir.join("a"),
            FoldCfg { block_target: 5 << 30, ..FoldCfg::default() }
        )
        .is_err(),
        "a block_target that would overflow the u32 segment append point must refuse"
    );
    assert!(
        turndb::fold::Fold::open(&dir.join("b"), FoldCfg { level: 99, ..FoldCfg::default() })
            .is_err(),
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
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .unwrap()
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
fn another_part_draft_epoch_is_refused_before_anything_is_parsed() {
    let dir = built("future");
    let p = part_path(&dir);
    edit_footer(&p, |f| f[45] = 200);
    let e = match turndb::part::Part::open(&p) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("another part draft epoch must not open"),
    };
    assert!(e.contains("draft epoch"), "expected an epoch refusal, got: {e}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_truncated_part_is_refused() {
    let dir = built("trunc");
    let p = part_path(&dir);
    let b = std::fs::read(&p).unwrap();
    for keep in [0usize, 8, 55, b.len() / 2] {
        std::fs::write(&p, &b[..keep.min(b.len())]).unwrap();
        assert!(
            turndb::part::Part::open(&p).is_err(),
            "a part truncated to {keep} bytes must be refused, not half-read"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_segment_with_unknown_flags_is_refused() {
    let dir = built("flags");
    let store = dir.join("s.turndb");
    // Bit 0 is ENCRYPTED and KNOWN — a known bit is acted on, not refused. Pick a bit no
    // revision has claimed, which is what "unknown means stop, not adapt" is actually about.
    // The segment lives as a member now; its extent is where the header bytes are.
    let (seg_off, _) = {
        let c = turndb::container::Container::open(&store).unwrap();
        let name = c
            .names()
            .map(String::from)
            .filter(|n| n.starts_with("fold/") && n.ends_with(".fold"))
            .min()
            .expect("a fold segment member");
        let extents = c.member_extents(&name).unwrap();
        extents[0]
    };
    let mut b = std::fs::read(&store).unwrap();
    let at = seg_off as usize + 12;
    b[at..at + 4].copy_from_slice(&(1u32 << 17).to_le_bytes());
    std::fs::write(&store, &b).unwrap();

    let e = match turndb::store::open_read_container(&store, FoldCfg::default()) {
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
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(
        &store,
        FoldCfg { seg_max: 1 << 17, block_target: 1 << 14, ..FoldCfg::default() },
    )
    .unwrap();
    for i in 0..300u32 {
        let body: Vec<u8> = (0..64u32)
            .flat_map(|j| blake3::hash(&(i * 500 + j).to_le_bytes()).as_bytes().to_vec())
            .collect();
        s.put(&format!("s{i:04}"), &[Span::Piece(&body)], vec![]).unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    s.close().unwrap();

    // Rebuild the store byte-identical except for segment 0 — a hole punched with the public
    // container writer, so the fixture is a well-formed CONTAINER whose FOLD is not dense.
    let source = turndb::container::Container::open(&store).unwrap();
    let seg_names: Vec<String> = source
        .names()
        .map(String::from)
        .filter(|n| n.starts_with("fold/") && n.ends_with(".fold"))
        .collect();
    assert!(seg_names.len() >= 3, "the fixture must produce several segments: {seg_names:?}");
    let hole = seg_names.iter().min().unwrap().clone();
    let gapped = dir.join("gapped.turndb");
    let mut fresh = turndb::container::Container::create(&gapped).unwrap();
    for name in source.names().map(String::from).collect::<Vec<_>>() {
        if name == hole {
            continue;
        }
        let bytes = source.read_file_bounded(&name, 1 << 30).unwrap();
        fresh.put_bytes(&name, &bytes).unwrap();
    }
    fresh.commit().unwrap();
    drop(fresh);

    assert!(
        turndb::store::open_read_container(&gapped, FoldCfg::default()).is_err(),
        "a reader must refuse a fold whose segments are not dense"
    );
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
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(&store, FoldCfg::default()).unwrap();
    // an empty literal is dropped rather than encoded, so the reserved codepoint never appears
    s.put("r", &[Span::Lit(b""), Span::Lit(b"real"), Span::Lit(b"")], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    assert_eq!(
        s.reconstruct("r").unwrap().unwrap(),
        b"real".to_vec(),
        "dropping an empty literal must preserve the body exactly"
    );
    s.close().unwrap();
    materialize(&store, &dir);

    // and the encoded program contains no zero tag
    let p = turndb::part::Part::open(&part_path(&dir)).unwrap();
    let ops = p.body(0).unwrap();
    assert_eq!(ops.len(), 1, "the two empty literals are gone, the real one remains: {ops:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_wal_frame_from_the_discarded_format_is_refused_not_silently_dropped() {
    // An unknown tag is ambiguous: a crash mid-append leaves garbage (the log ends), and a newer
    // writer's frame also lands here (refusing is the only safe reading). Treating both as "end of
    // log" meant a future frame type would silently discard every committed record after it. The crc
    // disambiguates — a torn tail does not checksum, a deliberate frame does.
    let dir = tmp("walfuture");
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("s.turndb");
    let mut s = Store::open_file(&store, FoldCfg::default()).unwrap();
    s.put("a", &[Span::Lit(b"first")], vec![]).unwrap();
    s.sync().unwrap();
    drop(s);

    // append a well-formed frame with an unknown tag
    let path = wal_sidecar(&store);
    let mut b = std::fs::read(&path).unwrap();
    let payload = b"discarded-format payload";
    let mut hdr = vec![0x5Cu8];
    hdr.extend_from_slice(&99u64.to_le_bytes());
    hdr.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    let mut h = crc32fast::Hasher::new();
    h.update(&hdr);
    h.update(payload);
    b.extend_from_slice(&hdr);
    b.extend_from_slice(payload);
    b.extend_from_slice(&h.finalize().to_le_bytes());
    std::fs::write(&path, &b).unwrap();

    let e = match Store::open_file(&store, FoldCfg::default()) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a checksumming frame with an unknown tag must be refused"),
    };
    assert!(e.contains("tag"), "expected a frame-tag refusal, got: {e}");

    // ...while a genuinely TORN tail still just ends the log, as it must
    let mut torn = std::fs::read(&path).unwrap();
    let n = torn.len();
    torn[n - 1] ^= 0xFF; // break the crc
    std::fs::write(&path, &torn).unwrap();
    let s = Store::open_file(&store, FoldCfg::default())
        .expect("a torn tail is the end of the log, not an error");
    assert_eq!(s.reconstruct("a").unwrap().unwrap(), b"first".to_vec());
    std::fs::remove_dir_all(&dir).ok();
}
