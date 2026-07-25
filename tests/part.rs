//! Step-2 gate: a part reconstructs every record BYTE-EXACT — body bytes, attribute order, and
//! duplicate keys included. This is where the cardinal invariant starts being enforced through the
//! columnar layer, so these tests are the ones that must never be weakened.

use std::path::PathBuf;
use turndb::fold::{Fold, FoldCfg};
use turndb::part::{self, Part};
use turndb::{AttrValue, BodyOp, Record};

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
            body: vec![BodyOp::Piece { hash: p1.hash, len: p1.loc.raw }],
            attrs: vec![
                ("z.last".into(), AttrValue::Str("v".into())),
                ("dup".into(), AttrValue::Int(1)),
                ("dup".into(), AttrValue::Int(2)),
                ("mixed".into(), AttrValue::Int(7)),
                ("f".into(), AttrValue::Float(nan)),
                ("g".into(), AttrValue::Float(-0.0)),
            ],
        },
        Record {
            id: "genai:aè#input".into(),
            body: vec![BodyOp::Piece { hash: p1.hash, len: p1.loc.raw }],
            // the SAME key at a different type — must become a separate column
            attrs: vec![("mixed".into(), AttrValue::Str("7".into())), ("g".into(), AttrValue::Float(0.0))],
        },
        Record {
            // interleaving that column storage alone cannot reproduce: a, b, a
            id: "rec:interleaved".into(),
            body: vec![
                BodyOp::Lit(b"[".to_vec()),
                BodyOp::Piece { hash: p2.hash, len: p2.loc.raw },
                BodyOp::Lit(b",".to_vec()),
                BodyOp::Piece { hash: p3.hash, len: p3.loc.raw },
                BodyOp::Lit(b"]".to_vec()),
            ],
            attrs: vec![
                ("a".into(), AttrValue::Str("one".into())),
                ("b".into(), AttrValue::Bool(true)),
                ("a".into(), AttrValue::Str("two".into())),
            ],
        },
        Record { id: "rec:no-attrs".into(), body: vec![BodyOp::Lit(b"bare".to_vec())], attrs: Vec::new() },
        Record {
            id: "rec:empty-body".into(),
            body: Vec::new(),
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

    // ids come back sorted, and every one is findable
    let ids = p.ids().unwrap();
    let mut want: Vec<String> = recs.iter().map(|r| r.id.clone()).collect();
    want.sort();
    assert_eq!(ids, want);

    for r in &recs {
        let row = p.find(&r.id).unwrap().expect("every id must be findable");
        let got = p.record(row).unwrap();
        assert_eq!(got.id, r.id);
        assert_eq!(got.body, r.body, "body program drifted for {}", r.id);
        assert_eq!(got.attrs, r.attrs, "ATTR DRIFT (order/dupes/types) for {}", r.id);

        // and the content itself reconstructs byte-exact out of the fold
        let mut expect = Vec::new();
        for op in &r.body {
            match op {
                BodyOp::Lit(b) => expect.extend_from_slice(b),
                BodyOp::Piece { hash, .. } => {
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
    let mut fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();
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
            body: Vec::new(),
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
        .map(|i| fold.put(format!("shared message body number {i}, with padding to give it size").as_bytes()).unwrap())
        .collect();
    let recs: Vec<Record> = (0..2000)
        .map(|i| Record {
            id: format!("genai:trace{:05}:span{:03}#input", i / 7, i % 7),
            body: (0..(i % 9 + 1))
                .map(|k| {
                    let p = &pieces[(i + k) % pieces.len()];
                    BodyOp::Piece { hash: p.hash, len: p.loc.raw }
                })
                .collect(),
            attrs: vec![
                ("model".into(), AttrValue::Str(format!("claude-{}", i % 3))),
                ("tokens".into(), AttrValue::Int((i * 13 % 997) as i64)),
                ("ok".into(), AttrValue::Bool(i % 2 == 0)),
            ],
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
        assert_eq!(got.body, r.body, "body drifted for {}", r.id);
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
    let mut fold = Fold::open(&dir.join("fold"), FoldCfg { block_target: 4096, ..Default::default() }).unwrap();
    let recs: Vec<Record> = (0..200)
        .map(|i| Record {
            id: format!("r{i:04}"),
            body: vec![{
                let p = fold.put(format!("piece {i} with enough bytes to matter for blocking").as_bytes()).unwrap();
                BodyOp::Piece { hash: p.hash, len: p.loc.raw }
            }],
            attrs: Vec::new(),
        })
        .collect();
    let path = dir.join("p.part");
    part::build(&path, &recs, 1, 1, 3, |h| fold.lookup(*h)).unwrap();
    let p = Part::open(&path).unwrap();
    let n = p.piece_count().unwrap();
    assert!(n > 1);
    let mut prev = (0u32, 0u32, 0u32);
    for i in 0..n {
        let (loc, _) = p.piece(i).unwrap();
        let key = (loc.seg, loc.block_off, loc.in_off);
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
    assert!(Part::open(&path).is_err(), "a part without its footer must be refused, never half-read");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn duplicate_ids_in_one_part_are_refused() {
    let dir = tmp("dupids");
    std::fs::create_dir_all(&dir).unwrap();
    let mut fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();
    let recs = vec![
        Record { id: "same".into(), body: Vec::new(), attrs: Vec::new() },
        Record { id: "same".into(), body: Vec::new(), attrs: Vec::new() },
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
