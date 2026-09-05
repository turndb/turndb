//! Step-2 gate: a part reconstructs every record BYTE-EXACT — body bytes, attribute order, and
//! duplicate keys included. This is where the cardinal invariant starts being enforced through the
//! columnar layer, so these tests are the ones that must never be weakened.

use std::path::PathBuf;
use turndb::fold::{Fold, FoldCfg};
use turndb::part::{self, Part};
use turndb::{AttrValue, Content, ContentOp, Record, BODY_CONTENT};

fn body_content(ops: Vec<ContentOp>) -> Vec<Content> {
    let identity = match ops.as_slice() {
        [ContentOp::Piece { hash, .. }] => turndb::ContentHash(hash.0),
        ops if ops.iter().all(|op| matches!(op, ContentOp::Lit(_))) => {
            let mut bytes = Vec::new();
            for op in ops {
                if let ContentOp::Lit(literal) = op {
                    bytes.extend_from_slice(literal);
                }
            }
            turndb::ContentHash::of(&bytes)
        }
        _ => panic!("mixed test content must state its reconstructed bytes"),
    };
    vec![Content::identified(BODY_CONTENT, ops, identity)]
}

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-part-{tag}-{}-{n}", std::process::id()))
}

/// A fixture exercising every shape that has historically broken columnar encodings.
fn fixture(fold: &mut Fold) -> Vec<Record> {
    let p1 = fold.put(b"a shared system prompt that many records reference").unwrap();
    let p2 = fold.put(b"a second distinct piece of content").unwrap();
    let p3 = fold.put(&"long body ".repeat(500).into_bytes()).unwrap();

    let nan = f64::from_bits(0x7ff8_0000_0000_0001);
    vec![
        Record {
            // duplicate keys, mixed types on one key, NaN, -0.0, and a non-ASCII id
            id: "genai:aé#input".into(),
            contents: body_content(vec![ContentOp::Piece { hash: p1.hash, len: p1.loc.raw }]),
            attrs: vec![
                ("z.last".into(), AttrValue::Str("v".into())),
                ("dup".into(), AttrValue::Int(1)),
                ("dup".into(), AttrValue::Int(2)),
                ("mixed".into(), AttrValue::Int(7)),
                ("f".into(), AttrValue::Float(nan)),
                ("g".into(), AttrValue::Float(-0.0)),
                ("u".into(), AttrValue::UInt(u64::MAX)),
                ("bin".into(), AttrValue::Bytes(vec![0, 0xff, 0x80, b'x'])),
                ("at".into(), AttrValue::TimestampNs(i64::MIN + 1)),
                ("nothing".into(), AttrValue::Null),
            ],
        },
        Record {
            id: "genai:aè#input".into(),
            contents: body_content(vec![ContentOp::Piece { hash: p1.hash, len: p1.loc.raw }]),
            // the SAME key at a different type — must become a separate column
            attrs: vec![
                ("mixed".into(), AttrValue::Str("7".into())),
                ("g".into(), AttrValue::Float(0.0)),
            ],
        },
        Record {
            // interleaving that column storage alone cannot reproduce: a, b, a
            id: "rec:interleaved".into(),
            contents: vec![Content::identified(
                BODY_CONTENT,
                vec![
                    ContentOp::Lit(b"[".to_vec()),
                    ContentOp::Piece { hash: p2.hash, len: p2.loc.raw },
                    ContentOp::Lit(b",".to_vec()),
                    ContentOp::Piece { hash: p3.hash, len: p3.loc.raw },
                    ContentOp::Lit(b"]".to_vec()),
                ],
                turndb::ContentHash::of(
                    &[
                        b"[".as_slice(),
                        b"a second distinct piece of content",
                        b",",
                        &"long body ".repeat(500).into_bytes(),
                        b"]",
                    ]
                    .concat(),
                ),
            )],
            attrs: vec![
                ("a".into(), AttrValue::Str("one".into())),
                ("b".into(), AttrValue::Bool(true)),
                ("a".into(), AttrValue::Str("two".into())),
            ],
        },
        Record {
            id: "rec:no-attrs".into(),
            contents: body_content(vec![ContentOp::Lit(b"bare".to_vec())]),
            attrs: Vec::new(),
        },
        Record {
            id: "rec:empty-body".into(),
            contents: body_content(Vec::new()),
            attrs: vec![("only".into(), AttrValue::Bool(false))],
        },
    ]
}

#[test]
fn records_round_trip_byte_exact() {
    let dir = tmp("roundtrip");
    std::fs::create_dir_all(&dir).unwrap();
    let mut fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();
    let recs = fixture(&mut fold);
    let path = dir.join("p.part");
    let meta = part::build(&path, &recs, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    fold.sync().unwrap();
    assert_eq!(meta.n_records as usize, recs.len());

    let p = Part::open(&path).unwrap();
    assert_eq!(p.len(), recs.len());
    assert!(
        turndb::part::attrs::read_row(&p, usize::MAX).is_err(),
        "a hostile public row index must refuse instead of overflowing"
    );

    // ids come back sorted, and every one is findable
    let ids = p.ids().unwrap();
    let mut want: Vec<String> = recs.iter().map(|r| r.id.clone()).collect();
    want.sort();
    assert_eq!(*ids, want);

    for r in &recs {
        let row = p.find(&r.id).unwrap().expect("every id must be findable");
        let got = p.record(row).unwrap();
        assert_eq!(got.id, r.id);
        assert_eq!(got.contents, r.contents, "content programs drifted for {}", r.id);
        assert_eq!(got.attrs, r.attrs, "ATTR DRIFT (order/dupes/types) for {}", r.id);

        // and the content itself reconstructs byte-exact out of the fold
        let mut expect = Vec::new();
        for op in &r.content(BODY_CONTENT).expect("body content").ops {
            match op {
                ContentOp::Lit(b) => expect.extend_from_slice(b),
                ContentOp::Piece { hash, .. } => {
                    expect.extend_from_slice(&fold.read(fold.lookup(*hash).unwrap()).unwrap())
                }
            }
        }
        assert_eq!(p.reconstruct(row, &fold).unwrap(), expect, "BYTE DRIFT for {}", r.id);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn float_bit_patterns_survive() {
    let dir = tmp("floats");
    std::fs::create_dir_all(&dir).unwrap();
    let fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();
    let odd = [
        f64::from_bits(0x7ff8_0000_0000_0001), // NaN with a payload
        -0.0,
        0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MIN_POSITIVE,
    ];
    let recs: Vec<Record> = odd
        .iter()
        .enumerate()
        .map(|(i, f)| Record {
            id: format!("f{i:03}"),
            contents: body_content(Vec::new()),
            attrs: vec![("v".into(), AttrValue::Float(*f))],
        })
        .collect();
    let path = dir.join("p.part");
    part::build(&path, &recs, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    let p = Part::open(&path).unwrap();
    for (i, f) in odd.iter().enumerate() {
        let row = p.find(&format!("f{i:03}")).unwrap().unwrap();
        match p.attrs(row).unwrap()[0].1 {
            AttrValue::Float(g) => assert_eq!(
                g.to_bits(),
                f.to_bits(),
                "float BIT PATTERN must survive (index {i}) — value equality is not enough"
            ),
            ref other => panic!("wrong type back: {other:?}"),
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn scales_to_many_records_with_shared_content() {
    let dir = tmp("scale");
    std::fs::create_dir_all(&dir).unwrap();
    let mut fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();

    // 2,000 records over a small shared piece pool — the dedup shape a real trace store has
    let pieces: Vec<_> = (0..40)
        .map(|i| {
            fold.put(
                format!("shared message body number {i}, with padding to give it size").as_bytes(),
            )
            .unwrap()
        })
        .collect();
    let recs: Vec<Record> = (0..2000)
        .map(|i| {
            let indexes: Vec<_> = (0..(i % 9 + 1)).map(|k| (i + k) % pieces.len()).collect();
            let ops = indexes
                .iter()
                .map(|&index| {
                    let p = &pieces[index];
                    ContentOp::Piece { hash: p.hash, len: p.loc.raw }
                })
                .collect();
            let bytes = indexes
                .iter()
                .flat_map(|index| {
                    format!("shared message body number {index}, with padding to give it size")
                        .into_bytes()
                })
                .collect::<Vec<_>>();
            Record {
                id: format!("genai:trace{:05}:span{:03}#input", i / 7, i % 7),
                contents: vec![Content::identified(
                    BODY_CONTENT,
                    ops,
                    turndb::ContentHash::of(&bytes),
                )],
                attrs: vec![
                    ("model".into(), AttrValue::Str(format!("claude-{}", i % 3))),
                    ("tokens".into(), AttrValue::Int((i * 13 % 997) as i64)),
                    ("ok".into(), AttrValue::Bool(i % 2 == 0)),
                ],
            }
        })
        .collect();

    let path = dir.join("p.part");
    part::build(&path, &recs, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    fold.sync().unwrap();
    let p = Part::open(&path).unwrap();
    assert_eq!(p.len(), recs.len());
    assert!(p.piece_count().unwrap() <= pieces.len(), "the dictionary holds only distinct pieces");

    for r in &recs {
        let row = p.find(&r.id).unwrap().expect("id findable");
        let got = p.record(row).unwrap();
        assert_eq!(got.attrs, r.attrs, "attrs drifted for {}", r.id);
        assert_eq!(got.contents, r.contents, "content drifted for {}", r.id);
        p.reconstruct(row, &fold).unwrap();
    }

    // absent ids are absent, not aliased to a neighbour
    for absent in ["", "genai:trace00000", "zzz", "genai:trace99999:span000#input"] {
        assert_eq!(p.find(absent).unwrap(), None, "{absent:?} must not resolve");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn piece_dictionary_is_in_fold_order() {
    // The merge operator's linear dictionary union and a scan's sequential fold reads both depend on
    // this ordering, so it is asserted rather than assumed.
    let dir = tmp("dictorder");
    std::fs::create_dir_all(&dir).unwrap();
    let mut fold =
        Fold::open(&dir.join("fold"), FoldCfg { block_target: 4096, ..Default::default() })
            .unwrap();
    let recs: Vec<Record> = (0..200)
        .map(|i| Record {
            id: format!("r{i:04}"),
            contents: body_content(vec![{
                let p = fold
                    .put(format!("piece {i} with enough bytes to matter for blocking").as_bytes())
                    .unwrap();
                ContentOp::Piece { hash: p.hash, len: p.loc.raw }
            }]),
            attrs: Vec::new(),
        })
        .collect();
    let path = dir.join("p.part");
    part::build(&path, &recs, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    let p = Part::open(&path).unwrap();
    let n = p.piece_count().unwrap();
    assert!(n > 1);
    let mut prev = (0u32, 0u32);
    for i in 0..n {
        let (loc, _) = p.piece(i).unwrap();
        let key = (loc.block_id, loc.in_off);
        assert!(key > prev || i == 0, "piece dictionary must ascend in fold order at {i}");
        prev = key;
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_torn_part_is_refused() {
    let dir = tmp("torn");
    std::fs::create_dir_all(&dir).unwrap();
    let mut fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();
    let recs = fixture(&mut fold);
    let path = dir.join("p.part");
    part::build(&path, &recs, 1, 1, 3, |h| fold.lookup(*h)).unwrap();

    // Truncate away the footer — exactly what a crash mid-write leaves behind.
    let len = std::fs::metadata(&path).unwrap().len();
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(len - 8).unwrap();
    drop(f);
    assert!(
        Part::open(&path).is_err(),
        "a part without its footer must be refused, never half-read"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn duplicate_ids_in_one_part_are_refused() {
    let dir = tmp("dupids");
    std::fs::create_dir_all(&dir).unwrap();
    let fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();
    let recs = vec![
        Record { id: "same".into(), contents: body_content(Vec::new()), attrs: Vec::new() },
        Record { id: "same".into(), contents: body_content(Vec::new()), attrs: Vec::new() },
    ];
    let path = dir.join("p.part");
    // A part is one version per id; cross-part resolution is by sequence range. Two versions inside
    // one part would make that resolution ambiguous, so it is refused at build.
    assert!(part::build(&path, &recs, 1, 1, 3, |h| fold.lookup(*h)).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn build_is_deterministic() {
    let dir = tmp("determinism");
    std::fs::create_dir_all(&dir).unwrap();
    let mut fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();
    let recs = fixture(&mut fold);
    let a = dir.join("a.part");
    let b = dir.join("b.part");
    part::build(&a, &recs, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    // same records, shuffled — column ordinals come from sorted (key, tag), not arrival order
    let mut shuffled = recs.clone();
    shuffled.reverse();
    part::build(&b, &shuffled, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "same records must produce byte-identical parts regardless of input order"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Format edges. Each of these was silent before: a wrong answer, not an error.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_toc_pointing_past_the_file_is_refused() {
    // The footer is checksummed; the TOC is not. A corrupt-but-plausible entry would otherwise send a
    // read to allocate `stored` bytes and read at an arbitrary offset.
    let d = tmp("toccorrupt");
    let path = d.join("p.part");
    let mut fold = Fold::open(&d.join("fold"), FoldCfg::default()).unwrap();
    let recs: Vec<Record> = (0..40)
        .map(|i| {
            let body = format!("record {i} with some content").into_bytes();
            let p = fold.put(&body).unwrap();
            Record {
                id: format!("k{i:03}"),
                contents: body_content(vec![ContentOp::Piece { hash: p.hash, len: p.loc.raw }]),
                attrs: vec![("v".into(), AttrValue::Int(i))],
            }
        })
        .collect();
    part::build(&path, &recs, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    fold.sync().unwrap();
    assert!(Part::open(&path).is_ok(), "the part must be sound to begin with");

    // Truncating the file leaves the footer intact only if we rebuild it, so instead corrupt a TOC
    // offset by rewriting the file shorter and repairing the footer checksum is overkill — simply
    // truncating makes the footer unreadable, which is already covered. Take the file and append
    // nothing, but shrink it: the TOC entries then point past the end.
    let good = std::fs::read(&path).unwrap();
    let mut short = good.clone();
    // keep the last FOOTER_LEN bytes (so the footer still verifies) but drop a chunk of the body
    let flen = part::FOOTER_LEN as usize;
    let cut = 4096.min(short.len() - flen - 1);
    let footer = short[short.len() - flen..].to_vec();
    short.truncate(short.len() - flen - cut);
    short.extend_from_slice(&footer);
    std::fs::write(&path, &short).unwrap();

    // Either the footer or the range check must reject it. What must NOT happen is a successful open
    // that later reads at a bogus offset.
    match Part::open(&path) {
        Err(_) => {}
        Ok(p) => {
            // if it opened, every section read must still refuse rather than return junk
            let mut any_ok = false;
            for r in 0..p.len().min(5) {
                if p.record(r).is_ok() {
                    any_ok = true;
                }
            }
            assert!(!any_ok, "a truncated part opened AND served records");
        }
    }
    std::fs::remove_dir_all(&d).ok();
}

// ---------------------------------------------------------------------------------------------
// Format levers. The fold could always refuse an unknown future; until now the part could not.
// ---------------------------------------------------------------------------------------------

fn a_part(d: &std::path::Path) -> (std::path::PathBuf, Fold) {
    let mut fold = Fold::open(&d.join("fold"), FoldCfg::default()).unwrap();
    let recs: Vec<Record> = (0..40)
        .map(|i| {
            let body = format!("record {i} with enough content to be worth folding").into_bytes();
            let p = fold.put(&body).unwrap();
            Record {
                id: format!("k{i:03}"),
                contents: body_content(vec![ContentOp::Piece { hash: p.hash, len: p.loc.raw }]),
                attrs: vec![("v".into(), AttrValue::Str(format!("value {i}")))],
            }
        })
        .collect();
    let path = d.join("v.part");
    part::build(&path, &recs, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    fold.sync().unwrap();
    (path, fold)
}

/// Rewrite the footer with `version`, repairing the checksum so only the version is under test.
fn set_version(path: &std::path::Path, version: u8) {
    let mut b = std::fs::read(path).unwrap();
    let n = b.len();
    let fl = part::FOOTER_LEN as usize;
    b[n - fl + 45] = version;
    let x = blake3::hash(&b[n - fl..n - 4]);
    b[n - 4..].copy_from_slice(&x.as_bytes()[0..4]);
    std::fs::write(path, &b).unwrap();
}

fn section_offset(bytes: &[u8], wanted: &str) -> usize {
    fn varint(bytes: &[u8], at: &mut usize) -> u64 {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = bytes[*at];
            *at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }

    let footer = bytes.len() - part::FOOTER_LEN as usize;
    let toc_offset =
        u64::from_le_bytes(bytes[footer + 8..footer + 16].try_into().unwrap()) as usize;
    let toc_stored =
        u32::from_le_bytes(bytes[footer + 16..footer + 20].try_into().unwrap()) as usize;
    let toc_raw = u32::from_le_bytes(bytes[footer + 20..footer + 24].try_into().unwrap()) as usize;
    let toc = match bytes[footer + 44] {
        0 => bytes[toc_offset..toc_offset + toc_stored].to_vec(),
        1 => zstd::bulk::decompress(&bytes[toc_offset..toc_offset + toc_stored], toc_raw).unwrap(),
        codec => panic!("unexpected TOC codec {codec}"),
    };
    let mut at = 0usize;
    let count = varint(&toc, &mut at);
    for _ in 0..count {
        let name_len = varint(&toc, &mut at) as usize;
        let name = std::str::from_utf8(&toc[at..at + name_len]).unwrap();
        at += name_len;
        let offset = varint(&toc, &mut at) as usize;
        let _stored = varint(&toc, &mut at);
        let _raw = varint(&toc, &mut at);
        at += 1; // codec
        at += 4; // stored-byte checksum
        if name == wanted {
            return offset;
        }
    }
    panic!("part has no section {wanted}");
}

#[test]
fn a_part_from_a_newer_writer_is_refused_not_misparsed() {
    let d = tmp("version");
    let (path, _fold) = a_part(&d);
    assert!(Part::open(&path).is_ok());

    set_version(&path, part::PART_DRAFT_EPOCH + 1);
    let e = match Part::open(&path) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a newer format version must not open"),
    };
    assert!(e.contains("draft epoch"), "expected an epoch refusal, got: {e}");

    // and the current version still opens, so the check is not simply rejecting everything
    set_version(&path, part::PART_DRAFT_EPOCH);
    assert!(Part::open(&path).is_ok());
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn a_part_from_the_discarded_format_is_refused() {
    let d = tmp("discarded-magic");
    let (path, _fold) = a_part(&d);
    let mut bytes = std::fs::read(&path).unwrap();
    let footer = bytes.len() - 56;
    bytes[footer..footer + 8].copy_from_slice(b"TURNPART");
    let digest = blake3::hash(&bytes[footer..bytes.len() - 4]);
    let end = bytes.len();
    bytes[end - 4..].copy_from_slice(&digest.as_bytes()[..4]);
    std::fs::write(&path, bytes).unwrap();
    assert!(Part::open(&path).is_err(), "the discarded part magic must not open");
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn every_section_carries_a_checksum_that_catches_a_flipped_bit() {
    let d = tmp("xsum");
    let (path, fold) = a_part(&d);
    let p = Part::open(&path).unwrap();
    let n = p.verify_sections().unwrap();
    assert!(n >= 8, "a part should have checksummed sections; got {n}");
    drop(p);

    // Flip one stored byte inside an advisory section not needed to establish the part's
    // structural schema.
    // The footer still verifies; explicit verification must detect the payload drift.
    let mut b = std::fs::read(&path).unwrap();
    let at = section_offset(&b, "zone");
    b[at] ^= 0xFF;
    std::fs::write(&path, &b).unwrap();

    let p = Part::open(&path).unwrap();
    assert!(p.verify_sections().is_err(), "a flipped byte in a section must be caught");
    drop(fold);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn a_damaged_zone_widens_to_no_pruning() {
    let d = tmp("zone-xsum");
    let (path, fold) = a_part(&d);
    let mut bytes = std::fs::read(&path).unwrap();
    let at = section_offset(&bytes, "zone");
    bytes[at] ^= 0xff;
    std::fs::write(&path, bytes).unwrap();

    let part = Part::open(&path).unwrap();
    assert_eq!(
        part.zone(0).unwrap(),
        None,
        "unverified advisory bytes must never become negative-pruning evidence"
    );
    drop(fold);
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn an_uncached_section_changed_after_open_is_never_silently_consumed() {
    // An already-open read view may outlive container retention and later encounter storage that
    // free-space punch deallocated. Every uncached section read must therefore prove its stored
    // checksum instead of decoding zeros or changed bytes into a different logical answer.
    let d = tmp("read-xsum");
    let (path, fold) = a_part(&d);
    let p = Part::open(&path).unwrap();
    let mut b = std::fs::read(&path).unwrap();
    let at = section_offset(&b, "ids");
    b[at] ^= 0xFF;
    std::fs::write(&path, &b).unwrap();

    assert!(p.ids().is_err(), "an ordinary read must detect changed section bytes");
    drop(fold);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn a_part_reads_identically_out_of_an_embedded_extent() {
    // A part is footer-addressed, so it can live at an offset inside a container and be read through
    // a bounded extent with no code knowing the difference. Answers are byte-identical, and no read
    // may wander outside the extent.
    let d = tmp("embedded");
    std::fs::create_dir_all(&d).unwrap();
    let mut fold = Fold::open(&d.join("fold"), FoldCfg::default()).unwrap();
    let records = fixture(&mut fold);
    let path = d.join("plain.part");
    part::build(&path, &records, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    fold.sync().unwrap();
    let part_bytes = std::fs::read(&path).unwrap();

    // Surround the part with unrelated member bytes. Only the extent bounds say where it is.
    let packish = d.join("packish.bin");
    let prefix = b"NOT-A-PART-PREFIX-0123456789".repeat(7);
    let mut whole = prefix.clone();
    whole.extend_from_slice(&part_bytes);
    whole.extend_from_slice(&b"TRAILING-GARBAGE".repeat(11));
    std::fs::write(&packish, &whole).unwrap();

    let plain = Part::open(&path).unwrap();
    let f = std::fs::File::open(&packish).unwrap();
    let extent = turndb::readat::Slice::new(f, prefix.len() as u64, part_bytes.len() as u64);
    let embedded =
        Part::open_reader(Box::new(extent), turndb::part::cache::SectionCache::shared()).unwrap();

    assert_eq!(plain.meta(), embedded.meta());
    assert_eq!(plain.ids().unwrap(), embedded.ids().unwrap());
    for r in 0..plain.len() {
        assert_eq!(
            plain.record(r).unwrap(),
            embedded.record(r).unwrap(),
            "row {r} must read identically out of the extent"
        );
        assert_eq!(
            plain.reconstruct(r, &fold).unwrap(),
            embedded.reconstruct(r, &fold).unwrap(),
            "row {r} content must reconstruct identically out of the extent"
        );
    }
    drop(fold);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn the_streaming_builder_is_byte_identical_to_build_full() {
    // The in-memory builder is the streaming builder's ORACLE: same rows, same universes, and the
    // two files must match byte for byte — encodings, section order, TOC, footer, everything.
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    let d = tmp("streambuild");
    std::fs::create_dir_all(&d).unwrap();
    let mut fold = Fold::open(&d.join("fold"), FoldCfg::default()).unwrap();
    let mut records = fixture(&mut fold);
    records.push(Record { id: "zzz:tomb".into(), contents: vec![], attrs: vec![] });
    let tombs: Vec<bool> = records.iter().map(|r| r.id == "zzz:tomb").collect();
    fold.sync().unwrap();

    let p1 = d.join("a.part");
    part::build_full(&p1, &records, &tombs, 3, 9, 3, |h| fold.lookup(*h), &HashMap::new()).unwrap();

    // The streaming side gets what a merge can cheaply know up front: the piece dictionary, the
    // column universe with string dictionaries — then rows in id order.
    let mut dict_map = HashMap::new();
    for r in &records {
        for content in &r.contents {
            for op in &content.ops {
                if let ContentOp::Piece { hash, .. } = op {
                    dict_map.entry(*hash).or_insert_with(|| fold.lookup(*hash).unwrap());
                }
            }
        }
    }
    let dict: Vec<_> = dict_map.into_iter().map(|(h, l)| (l, h)).collect();
    let mut cols: BTreeMap<(String, u8), BTreeSet<Vec<u8>>> = BTreeMap::new();
    for r in &records {
        for (k, v) in &r.attrs {
            let e = cols.entry((k.clone(), v.type_tag())).or_default();
            match v {
                AttrValue::Str(s) => {
                    e.insert(s.as_bytes().to_vec());
                }
                AttrValue::Bytes(bytes) => {
                    e.insert(bytes.clone());
                }
                _ => {}
            }
        }
    }
    let columns: Vec<(String, u8)> = cols.keys().cloned().collect();
    let dicts: Vec<Vec<Vec<u8>>> = cols.values().map(|s| s.iter().cloned().collect()).collect();
    let content_names: BTreeSet<String> =
        records.iter().flat_map(|r| r.contents.iter().map(|c| c.name.clone())).collect();

    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| records[a].id.cmp(&records[b].id));
    let p2 = d.join("b.part");
    let mut b = turndb::part::builder::StreamBuilder::new(
        &p2,
        3,
        dict,
        content_names.into_iter().collect(),
        columns,
        dicts,
    )
    .unwrap();
    for &i in &order {
        let r = &records[i];
        b.push(r.id.as_bytes(), tombs[i], &r.contents, &r.attrs).unwrap();
    }
    b.finish(3, 9).unwrap();

    assert_eq!(
        std::fs::read(&p1).unwrap(),
        std::fs::read(&p2).unwrap(),
        "the streaming builder must be BYTE-IDENTICAL to build_full"
    );
    assert!(
        !std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".s")),
        "spools must be cleaned up"
    );
    drop(fold);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn streaming_builder_emits_only_self_openable_current_parts() {
    let d = tmp("stream-builder-boundary");
    std::fs::create_dir_all(&d).unwrap();

    let invalid = d.join("invalid.part");
    let error = turndb::part::builder::StreamBuilder::new(
        &invalid,
        3,
        vec![(turndb::fold::Loc { block_id: 0, in_off: 0, raw: 0 }, turndb::PieceHash::of(b""))],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .err()
    .expect("a zero-length physical dictionary entry must be refused");
    assert!(error.to_string().contains("zero-length"));
    assert!(!invalid.exists(), "argument refusal must precede artifact creation");

    let over_budget = d.join("over-budget.part");
    let limits =
        turndb::read_limits::ReadLimits { max_decoded_frame_bytes: 16, ..Default::default() };
    let error = turndb::part::builder::StreamBuilder::new_with_limits(
        &over_budget,
        3,
        vec![(turndb::fold::Loc { block_id: 0, in_off: 0, raw: 1 }, turndb::PieceHash::of(b"x"))],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        limits,
    )
    .err()
    .expect("derived piece-hash expansion must be admitted before allocation");
    assert!(error.to_string().contains("pdict.hash"), "unexpected refusal: {error:#}");
    assert!(!over_budget.exists(), "derived-section refusal must precede artifact creation");

    let valid = d.join("valid.part");
    let mut builder = turndb::part::builder::StreamBuilder::new(
        &valid,
        3,
        Vec::new(),
        vec![BODY_CONTENT.into()],
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert!(builder.push(&[0xff], false, &[], &[]).is_err());
    assert!(builder
        .push(b"record", false, &[Content::new(BODY_CONTENT, Vec::new())], &[])
        .is_err());
    let missing = turndb::PieceHash::of(b"x");
    assert!(builder
        .push(
            b"record",
            false,
            &[Content::identified(
                BODY_CONTENT,
                vec![ContentOp::Piece { hash: missing, len: 1 }],
                turndb::ContentHash(missing.0),
            )],
            &[],
        )
        .is_err());
    builder
        .push(
            b"record",
            false,
            &[Content::identified(BODY_CONTENT, Vec::new(), turndb::ContentHash::of(b""))],
            &[],
        )
        .unwrap();
    builder.finish(1, 1).unwrap();
    let opened = Part::open(&valid).unwrap();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened.record(0).unwrap().id, "record");

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn zone_maps_bound_columns_and_refuse_to_lie() {
    let d = tmp("zones");
    std::fs::create_dir_all(&d).unwrap();
    let fold = Fold::open(&d.join("fold"), FoldCfg::default()).unwrap();
    let rec = |id: &str, n: i64, t: f64, nanf: f64| Record {
        id: id.into(),
        contents: body_content(vec![ContentOp::Lit(b"x".to_vec())]),
        attrs: vec![
            ("n".into(), AttrValue::Int(n)),
            ("t".into(), AttrValue::Float(t)),
            ("nanf".into(), AttrValue::Float(nanf)),
            ("ok".into(), AttrValue::Bool(true)),
            ("s".into(), AttrValue::Str(format!("v{n}"))),
        ],
    };
    let records =
        vec![rec("a", 5, 1.5, 0.0), rec("b", -3, 2.5, f64::NAN), rec("c", 42, -9.25, 1.0)];
    let path = d.join("z.part");
    part::build(&path, &records, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    let p = Part::open(&path).unwrap();

    // colmeta ordinals are sorted (key, tag): n=0, nanf=1, ok=2, s=3, t=4
    assert_eq!(p.zone(0).unwrap(), Some((AttrValue::Int(-3), AttrValue::Int(42))));
    assert_eq!(p.zone(1).unwrap(), None, "a NaN anywhere makes a float column unprunable");
    assert_eq!(p.zone(2).unwrap(), Some((AttrValue::Bool(true), AttrValue::Bool(true))));
    assert_eq!(
        p.zone(3).unwrap(),
        None,
        "strings carry no zone — the dictionary already bounds them"
    );
    assert_eq!(p.zone(4).unwrap(), Some((AttrValue::Float(-9.25), AttrValue::Float(2.5))));
    assert_eq!(p.zone(9).unwrap(), None, "an out-of-range ordinal is no pruning, not an error");
    drop(fold);
    std::fs::remove_dir_all(&d).ok();
}
