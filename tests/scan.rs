use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::scan::{
    CancellationToken, Compare, ContentMode, ContentSelect, Direction, Predicate, ScanInputError,
    ScanInterrupted, ScanInterruptionReason, ScanRequest,
};
use turndb::store::{ContentSpans, Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-scan-{tag}-{}-{n}", std::process::id()))
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn cfg() -> FoldCfg {
    FoldCfg { seg_max: 1 << 22, ..FoldCfg::default() }
}

fn status(value: &str) -> Vec<(String, AttrValue)> {
    vec![("status".into(), AttrValue::Str(value.into()))]
}

/// The flushed part's home inside the single file: `(store path, extent offset, length)` of the
/// first part member, so read-fatal damage lands on the member's exact bytes.
fn part_span(dir: &std::path::Path) -> (PathBuf, u64, u64) {
    let file = store_file(dir);
    let c = turndb::container::Container::open(&file).unwrap();
    let name = c
        .names()
        .find(|n| n.starts_with("part-") && n.ends_with(".part"))
        .expect("a flushed part")
        .to_string();
    let extents = c.member_extents(&name).unwrap();
    assert_eq!(extents.len(), 1, "a part is staged whole and stays one extent");
    (file, extents[0].0, extents[0].1)
}

fn section_location(bytes: &[u8], wanted: &str) -> (usize, u8) {
    let footer = bytes.len() - 56;
    let toc_off = u64::from_le_bytes(bytes[footer + 8..footer + 16].try_into().unwrap()) as usize;
    let toc_stored =
        u32::from_le_bytes(bytes[footer + 16..footer + 20].try_into().unwrap()) as usize;
    let toc_raw = u32::from_le_bytes(bytes[footer + 20..footer + 24].try_into().unwrap());
    let toc_codec = bytes[footer + 44];
    let toc = turndb::fold::codec::decode(
        toc_codec,
        &bytes[toc_off..toc_off + toc_stored],
        toc_raw,
        None,
    )
    .unwrap();
    let mut at = 0usize;
    let count = turndb::part::idcol::get_varint(&toc, &mut at).unwrap();
    for _ in 0..count {
        let name_len = turndb::part::idcol::get_varint(&toc, &mut at).unwrap() as usize;
        let name = std::str::from_utf8(&toc[at..at + name_len]).unwrap();
        at += name_len;
        let offset = turndb::part::idcol::get_varint(&toc, &mut at).unwrap() as usize;
        let _stored = turndb::part::idcol::get_varint(&toc, &mut at).unwrap();
        let _raw = turndb::part::idcol::get_varint(&toc, &mut at).unwrap();
        let codec = toc[at];
        at += 1;
        at += 4; // stored-section checksum
        if name == wanted {
            return (offset, codec);
        }
    }
    panic!("part has no section {wanted:?}");
}

fn corrupt_sections(span: &(PathBuf, u64, u64), names: &[&str]) {
    let (path, off, len) = span;
    let mut file_bytes = std::fs::read(path).unwrap();
    let (off, len) = (*off as usize, *len as usize);
    let mut bytes = file_bytes[off..off + len].to_vec();
    for name in names {
        let (offset, codec) = section_location(&bytes, name);
        assert_ne!(codec, 0, "test section {name} must be compressed so damage is read-fatal");
        bytes[offset] ^= 0x80;
    }
    file_bytes[off..off + len].copy_from_slice(&bytes);
    std::fs::write(path, file_bytes).unwrap();
}

#[test]
fn extended_scalar_predicates_distinguish_explicit_null_from_missing() {
    let dir = tmp("extended-scalars");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    store
        .put_body(
            "a",
            b"",
            vec![
                ("u".into(), AttrValue::UInt(u64::MAX)),
                ("raw".into(), AttrValue::Bytes(vec![0, 0xff])),
                ("at".into(), AttrValue::TimestampNs(-1)),
                ("maybe".into(), AttrValue::Null),
            ],
        )
        .unwrap();
    store
        .put_body(
            "b",
            b"",
            vec![
                ("u".into(), AttrValue::UInt(4)),
                ("raw".into(), AttrValue::Bytes(vec![1])),
                ("at".into(), AttrValue::TimestampNs(10)),
            ],
        )
        .unwrap();
    store.sync().unwrap();
    store.flush().unwrap();

    let cases = [
        Predicate::Attr { name: "u".into(), op: Compare::Gt, value: AttrValue::UInt(4) },
        Predicate::Attr { name: "raw".into(), op: Compare::Lt, value: AttrValue::Bytes(vec![1]) },
        Predicate::Attr { name: "at".into(), op: Compare::Lt, value: AttrValue::TimestampNs(0) },
        Predicate::Attr { name: "maybe".into(), op: Compare::Eq, value: AttrValue::Null },
        Predicate::AttrExists { name: "maybe".into(), present: true },
    ];
    for predicate in cases {
        let page = store
            .scan(&ScanRequest {
                attrs: vec!["u".into(), "raw".into(), "at".into(), "maybe".into()],
                predicates: vec![predicate],
                ..ScanRequest::default()
            })
            .unwrap();
        assert_eq!(page.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["a"]);
        assert!(page.rows[0].attrs.contains(&("maybe".into(), AttrValue::Null)));
    }
    let missing = store
        .scan(&ScanRequest {
            predicates: vec![Predicate::AttrExists { name: "maybe".into(), present: false }],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(missing.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["b"]);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn structured_predicates_prune_disjoint_part_zones_before_value_reads() {
    let dir = tmp("predicate-zone-pruning");
    let _cleanup = RemoveOnDrop(dir.clone());
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for index in 0..100i64 {
        store
            .put_body(
                &format!("a{index:03}"),
                b"cold",
                vec![
                    ("score".into(), AttrValue::Int(index)),
                    ("tag".into(), AttrValue::Str("cold".into())),
                ],
            )
            .unwrap();
    }
    store.sync().unwrap();
    store.flush().unwrap();
    for index in 200..210i64 {
        store
            .put_body(
                &format!("z{index:03}"),
                b"hot",
                vec![
                    ("score".into(), AttrValue::Int(index)),
                    ("tag".into(), AttrValue::Str("hot".into())),
                ],
            )
            .unwrap();
    }
    store.sync().unwrap();
    store.flush().unwrap();

    let page = store
        .scan(&ScanRequest {
            limit: 1000,
            attrs: vec!["score".into()],
            predicates: vec![Predicate::Attr {
                name: "score".into(),
                op: Compare::Gt,
                value: AttrValue::Int(150),
            }],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(page.rows.len(), 10);
    assert_eq!(page.stats.examined, 110);
    assert_eq!(page.stats.predicate_pruned_rows, 100);
    assert!(page.rows.iter().all(|row| row.id.starts_with('z')));

    for predicate in [
        Predicate::Attr {
            name: "tag".into(),
            op: Compare::Eq,
            value: AttrValue::Str("dictionary-proof".into()),
        },
        Predicate::AttrExists { name: "missing".into(), present: true },
        Predicate::ContentExists { name: "missing".into(), present: true },
    ] {
        let impossible = store
            .scan(&ScanRequest {
                limit: 1000,
                predicates: vec![predicate],
                ..ScanRequest::default()
            })
            .unwrap();
        assert!(impossible.rows.is_empty());
        assert_eq!(impossible.stats.predicate_pruned_rows, 110);
    }
}

#[test]
fn structured_projection_never_opens_unselected_attribute_or_content_columns() {
    let dir = tmp("projected-sections");
    let _cleanup = RemoveOnDrop(dir.clone());
    {
        let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
        let request = "request".repeat(100);
        let response = "response".repeat(100);
        let untouched = "never decode this sibling".repeat(20);
        for index in 0..256 {
            store
                .put_record(
                    &format!("x{index:03}"),
                    &[
                        ContentSpans::new("request", vec![Span::Lit(request.as_bytes())]),
                        ContentSpans::new("response", vec![Span::Lit(response.as_bytes())]),
                    ],
                    vec![
                        ("selected".into(), AttrValue::Int(index)),
                        ("untouched".into(), AttrValue::Str(untouched.clone())),
                    ],
                )
                .unwrap();
        }
        store.sync().unwrap();
        store.flush().unwrap();
    }

    // Column ordinals are sorted by name, as are content ordinals. Damage one sibling in each
    // namespace after open metadata was committed. A whole-record point decode would fail below.
    corrupt_sections(&part_span(&dir), &["col.val.1", "con.prog.1"]);
    let reader = turndb::store::open_read_container(&store_file(&dir), cfg()).unwrap();
    let selected = reader
        .scan(&ScanRequest {
            attrs: vec!["selected".into()],
            contents: vec![ContentSelect { name: "request".into(), mode: ContentMode::Metadata }],
            limit: 1,
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(selected.rows[0].attrs, [("selected".into(), AttrValue::Int(0))]);
    assert_eq!(selected.rows[0].contents[0].name, "request");

    let explanation = reader
        .explain_scan(&ScanRequest {
            attrs: vec!["untouched".into()],
            contents: vec![ContentSelect { name: "response".into(), mode: ContentMode::Metadata }],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(explanation.physical.immutable_rows_in_bounds, 256);

    assert!(
        reader
            .scan(&ScanRequest { attrs: vec!["untouched".into()], ..ScanRequest::default() })
            .is_err(),
        "selecting the damaged attribute column must read and reject it"
    );
    assert!(
        reader
            .scan(&ScanRequest {
                contents: vec![ContentSelect {
                    name: "response".into(),
                    mode: ContentMode::Metadata,
                }],
                ..ScanRequest::default()
            })
            .is_err(),
        "selecting the damaged content column must read and reject it"
    );
}

#[test]
fn grouped_projection_restores_cross_part_order_and_exact_row_shape() {
    let dir = tmp("grouped-projection");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    store
        .put_record(
            "a",
            &[ContentSpans::new("request", vec![Span::Lit(b"a-request")])],
            vec![
                ("x".into(), AttrValue::Int(1)),
                ("y".into(), AttrValue::Bool(true)),
                ("x".into(), AttrValue::Int(2)),
            ],
        )
        .unwrap();
    store
        .put_record(
            "c",
            &[ContentSpans::new("response", vec![Span::Lit(b"old-c")])],
            vec![("x".into(), AttrValue::Int(3))],
        )
        .unwrap();
    store.put_body("e", b"ignored", vec![("y".into(), AttrValue::Bool(false))]).unwrap();
    store.sync().unwrap();
    store.flush().unwrap();

    store
        .put_record(
            "b",
            &[
                ContentSpans::new("request", vec![Span::Lit(b"b-request")]),
                ContentSpans::new("response", vec![Span::Lit(b"b-response")]),
            ],
            vec![("y".into(), AttrValue::Bool(false)), ("x".into(), AttrValue::Int(4))],
        )
        .unwrap();
    store
        .put_record(
            "c",
            &[ContentSpans::new("response", vec![Span::Lit(b"new-c")])],
            vec![
                ("x".into(), AttrValue::Int(5)),
                ("x".into(), AttrValue::Int(6)),
                ("y".into(), AttrValue::Bool(true)),
            ],
        )
        .unwrap();
    store
        .put_record(
            "d",
            &[ContentSpans::new("request", vec![Span::Lit(b"d-request")])],
            vec![("x".into(), AttrValue::Int(7))],
        )
        .unwrap();
    store.sync().unwrap();
    store.flush().unwrap();

    let reader = turndb::store::open_read_container(&store_file(&dir), cfg()).unwrap();
    let request = ScanRequest {
        attrs: vec!["x".into(), "y".into()],
        contents: vec![
            ContentSelect { name: "request".into(), mode: ContentMode::Metadata },
            ContentSelect { name: "response".into(), mode: ContentMode::Metadata },
        ],
        ..ScanRequest::default()
    };
    let forward = reader.scan(&request).unwrap();
    assert_eq!(
        forward.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        ["a", "b", "c", "d", "e"]
    );
    assert_eq!(
        forward.rows[0].attrs,
        [
            ("x".into(), AttrValue::Int(1)),
            ("y".into(), AttrValue::Bool(true)),
            ("x".into(), AttrValue::Int(2)),
        ]
    );
    assert_eq!(
        forward.rows[2].attrs,
        [
            ("x".into(), AttrValue::Int(5)),
            ("x".into(), AttrValue::Int(6)),
            ("y".into(), AttrValue::Bool(true)),
        ],
        "newest row and duplicate occurrence order must survive per-part grouping"
    );
    assert_eq!(
        forward
            .rows
            .iter()
            .map(|row| row.contents.iter().map(|value| value.present).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [
            vec![true, false],
            vec![true, true],
            vec![false, true],
            vec![true, false],
            vec![false, false],
        ]
    );

    let mut reverse_request = request;
    reverse_request.direction = Direction::Reverse;
    let reverse = reader.scan(&reverse_request).unwrap();
    assert_eq!(
        reverse.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        ["e", "d", "c", "b", "a"]
    );
    assert_eq!(reverse.rows[2].attrs, forward.rows[2].attrs);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn grouped_projection_does_not_decode_past_a_full_page() {
    let dir = tmp("grouped-projection-demand");
    {
        let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
        store.put_body("a", b"", vec![("x".into(), AttrValue::Int(1))]).unwrap();
        store
            .put_body(
                "b",
                b"",
                vec![("x".into(), AttrValue::Str("damaged later type".repeat(200)))],
            )
            .unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
    }

    // `(name, type)` columns sort strings before integers. The first page needs only integer col 1;
    // a gather that speculatively included b would open the damaged string dictionary in col 0.
    corrupt_sections(&part_span(&dir), &["col.dict.0"]);
    let reader = turndb::store::open_read_container(&store_file(&dir), cfg()).unwrap();
    let first = reader
        .scan(&ScanRequest { attrs: vec!["x".into()], limit: 1, ..ScanRequest::default() })
        .unwrap();
    assert_eq!(first.rows[0].id, "a");
    assert_eq!(first.rows[0].attrs, [("x".into(), AttrValue::Int(1))]);

    reader
        .scan(&ScanRequest {
            attrs: vec!["x".into()],
            limit: 1,
            cursor: first.next,
            ..ScanRequest::default()
        })
        .unwrap_err();
    std::fs::remove_dir_all(dir).ok();
}

/// The demand bound must be recomputed per chunk, not fixed when the gather returns. With limit 2,
/// the first chunk is [a, b]; the predicate rejects b, so remaining demand is ONE row — a chunk
/// still sized two would project [c, d] together and open d's damaged column for a page that c
/// alone completes. A page the caller asked for must not fail on corruption in a row it never
/// needed; the row IS still corrupt, so the page that genuinely needs d must fail.
#[test]
fn read_ahead_chunks_shrink_to_remaining_demand_after_rejections() {
    let dir = tmp("shrinking-chunk-demand");
    {
        let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
        store.put_body("a", b"", vec![("x".into(), AttrValue::Int(1))]).unwrap();
        store.put_body("b", b"", vec![("x".into(), AttrValue::Int(2))]).unwrap();
        store.put_body("c", b"", vec![("x".into(), AttrValue::Int(3))]).unwrap();
        store
            .put_body(
                "d",
                b"",
                vec![("x".into(), AttrValue::Str("damaged later type".repeat(200)))],
            )
            .unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
    }

    corrupt_sections(&part_span(&dir), &["col.dict.0"]);
    let reader = turndb::store::open_read_container(&store_file(&dir), cfg()).unwrap();
    let request = ScanRequest {
        attrs: vec!["x".into()],
        predicates: vec![Predicate::Attr {
            name: "x".into(),
            op: Compare::Ne,
            value: AttrValue::Int(2),
        }],
        limit: 2,
        ..ScanRequest::default()
    };
    let first = reader.scan(&request).unwrap();
    assert_eq!(first.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["a", "c"]);
    assert_eq!(first.rows[0].attrs, [("x".into(), AttrValue::Int(1))]);
    assert_eq!(first.rows[1].attrs, [("x".into(), AttrValue::Int(3))]);
    assert!(first.next.is_some(), "d is still in range, so the page must not claim completion");

    reader.scan(&ScanRequest { cursor: first.next, ..request }).unwrap_err();
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn scan_explanation_shares_cursor_field_and_physical_scope_planning() {
    let dir = tmp("explain");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for id in ["a", "b", "c"] {
        store
            .put_record(
                id,
                &[ContentSpans::new("response", vec![Span::Lit(id.as_bytes())])],
                vec![("out".into(), AttrValue::Int(1))],
            )
            .unwrap();
    }
    store.sync().unwrap();
    store.flush().unwrap();
    store.put_body("b", b"new", vec![("x".into(), AttrValue::Bool(true))]).unwrap();
    store.delete("c").unwrap();
    store
        .put_record(
            "d",
            &[ContentSpans::new("payload", vec![Span::Lit(b"payload")])],
            vec![("y".into(), AttrValue::Bool(true))],
        )
        .unwrap();
    store.sync().unwrap();
    store.flush().unwrap();
    store.put_body("bb", b"staged", vec![]).unwrap();
    store.delete("d").unwrap();
    store.put_body("e", b"excluded", vec![]).unwrap();

    let request = ScanRequest {
        from: Some("b".into()),
        to: Some("e".into()),
        attrs: vec!["out".into()],
        contents: vec![
            ContentSelect { name: "response".into(), mode: ContentMode::Metadata },
            ContentSelect { name: "payload".into(), mode: ContentMode::Bytes },
        ],
        predicates: vec![
            Predicate::Id { op: Compare::GtEq, value: "b".into() },
            Predicate::AttrExists { name: "x".into(), present: true },
            Predicate::AttrExists { name: "y".into(), present: false },
            Predicate::ContentExists { name: "request".into(), present: false },
        ],
        limit: 7,
        max_examined: 19,
        max_resolution_entries: 23,
        max_reconstructed_bytes: 29,
        ..ScanRequest::default()
    };
    let explanation = store.explain_scan(&request).unwrap();
    assert_eq!(explanation.effective_from.as_deref(), Some("b"));
    assert_eq!(explanation.effective_to.as_deref(), Some("e"));
    assert_eq!(explanation.projected_attrs, ["out"]);
    assert_eq!(explanation.required_attrs, ["out", "x", "y"]);
    assert_eq!(explanation.predicate_only_attrs, ["x", "y"]);
    assert_eq!(
        explanation
            .projected_contents
            .iter()
            .map(|content| (content.name.as_str(), content.mode))
            .collect::<Vec<_>>(),
        [("response", ContentMode::Metadata), ("payload", ContentMode::Bytes)]
    );
    assert_eq!(explanation.required_contents, ["payload", "request", "response"]);
    assert_eq!(explanation.predicate_only_contents, ["request"]);
    assert_eq!(explanation.reconstructed_contents, ["payload"]);
    assert_eq!(
        (explanation.id_predicates, explanation.attr_predicates, explanation.content_predicates),
        (1, 2, 1)
    );
    assert_eq!(
        (explanation.limit, explanation.max_examined),
        (request.limit, request.max_examined)
    );
    assert_eq!(explanation.max_resolution_entries, 23);
    assert_eq!(explanation.max_reconstructed_bytes, 29);
    assert_eq!(explanation.physical.immutable_parts_considered, 2);
    assert_eq!(explanation.physical.immutable_parts_with_rows, 2);
    assert_eq!(explanation.physical.immutable_rows_in_bounds, 5);
    assert_eq!(explanation.physical.memtable_entries_in_bounds, 2);

    let reader = turndb::store::open_read_container(&store_file(&dir), cfg()).unwrap();
    let immutable = reader.explain_scan(&request).unwrap();
    assert_eq!(immutable.physical.memtable_entries_in_bounds, 0);
    assert_eq!(immutable.physical.immutable_rows_in_bounds, 5);

    let page_request = ScanRequest {
        from: Some("b".into()),
        to: Some("e".into()),
        limit: 1,
        ..ScanRequest::default()
    };
    let first = store.scan(&page_request).unwrap();
    let mut continuation = page_request;
    continuation.cursor = first.next;
    let resumed = store.explain_scan(&continuation).unwrap();
    assert!(resumed.uses_cursor);
    assert_eq!(resumed.effective_from.as_deref(), Some("b\0"));

    let mut invalid = continuation;
    invalid.from = Some("c".into());
    assert!(
        store.explain_scan(&invalid).is_err(),
        "explain and execution must check cursors alike"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn page_io_stats_are_operation_local_and_distinguish_cold_from_cached_reads() {
    let dir = tmp("io-stats");
    let cfg = FoldCfg { block_target: 4 << 10, ..cfg() };
    let payload = vec![b'x'; 8 << 10];
    {
        let mut store = Store::open_file(&store_file(&dir), cfg).unwrap();
        store
            .put_record(
                "a",
                &[ContentSpans::new("payload", vec![Span::Piece(&payload)])],
                status("ok"),
            )
            .unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
    }

    let reader = turndb::store::open_read_container(&store_file(&dir), cfg).unwrap();
    let request = ScanRequest {
        attrs: vec!["status".into()],
        contents: vec![ContentSelect { name: "payload".into(), mode: ContentMode::Bytes }],
        ..ScanRequest::default()
    };
    let cold = reader.scan(&request).unwrap();
    assert_eq!(cold.rows[0].contents[0].bytes.as_deref(), Some(payload.as_slice()));
    assert!(cold.stats.io.part_sections_touched > 0);
    assert!(cold.stats.io.part_section_cache_misses > 0);
    assert!(
        cold.stats.io.part_section_cache_hits <= 3,
        "resolved rows and projected content programs must not be point-located or decoded again"
    );
    assert!(cold.stats.io.part_stored_bytes_read > 0);
    assert!(cold.stats.io.part_raw_bytes_decoded >= cold.stats.io.part_stored_bytes_read);
    assert_eq!(cold.stats.io.fold_blocks_touched, 1);
    assert_eq!(cold.stats.io.fold_block_cache_misses, 1);
    assert_eq!(cold.stats.io.fold_block_cache_hits, 0);
    assert!(cold.stats.io.fold_stored_bytes_read > 0);
    assert!(cold.stats.io.fold_raw_bytes_decoded >= payload.len() as u64);

    let warm = reader.scan(&request).unwrap();
    assert_eq!(warm.rows, cold.rows);
    assert_eq!(warm.stats.io.fold_blocks_touched, 1);
    assert_eq!(warm.stats.io.fold_block_cache_hits, 1);
    assert_eq!(warm.stats.io.fold_block_cache_misses, 0);
    assert_eq!(warm.stats.io.fold_stored_bytes_read, 0);
    assert_eq!(warm.stats.io.fold_raw_bytes_decoded, 0);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn cancellation_and_deadlines_are_typed_and_return_no_partial_page() {
    let dir = tmp("interruption");
    let store = Store::open_file(&store_file(&dir), cfg()).unwrap();

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = store
        .scan(&ScanRequest { cancellation: Some(cancellation), ..ScanRequest::default() })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ScanInterrupted>().unwrap().reason,
        ScanInterruptionReason::Cancelled
    );

    let error = store
        .scan(&ScanRequest { deadline: Some(std::time::Instant::now()), ..ScanRequest::default() })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ScanInterrupted>().unwrap().reason,
        ScanInterruptionReason::DeadlineExceeded
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn writer_scan_is_live_projected_bounded_and_newest_first() {
    let dir = tmp("live");
    let mut writer = Store::open_file(&store_file(&dir), cfg()).unwrap();
    writer.put_body("b", b"old body", status("old")).unwrap();
    writer.sync().unwrap();
    writer.flush().unwrap();
    let reader = turndb::store::open_read_container(&store_file(&dir), cfg()).unwrap();

    // Unflushed overlay: one new id, one replacement, and one staged deletion.
    writer.put_record("a", &[ContentSpans::new("request", vec![])], status("new")).unwrap();
    writer
        .put_record(
            "b",
            &[ContentSpans::new("request", vec![Span::Piece(b"new request")])],
            vec![
                ("status".into(), AttrValue::Str("new".into())),
                ("tag".into(), AttrValue::Int(1)),
                ("tag".into(), AttrValue::Int(2)),
            ],
        )
        .unwrap();
    writer.put_body("c", b"doomed", status("new")).unwrap();
    writer.delete("c").unwrap();

    let mut request = ScanRequest {
        limit: 1,
        attrs: vec!["status".into(), "tag".into()],
        contents: vec![ContentSelect { name: "request".into(), mode: ContentMode::Metadata }],
        predicates: vec![Predicate::Attr {
            name: "status".into(),
            op: Compare::Eq,
            value: AttrValue::Str("new".into()),
        }],
        ..ScanRequest::default()
    };
    let first = writer.scan(&request).unwrap();
    assert_eq!(first.rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["a"]);
    assert!(first.next.is_some());
    assert_eq!(first.stats.content_values_reconstructed, 0);
    assert!(first.rows[0].contents[0].present);
    assert_eq!(first.rows[0].contents[0].len, Some(0));
    assert_eq!(first.rows[0].contents[0].bytes, None);

    // Projection is not part of cursor eligibility, so the next page may ask for bytes.
    request.cursor = first.next;
    request.contents[0].mode = ContentMode::Bytes;
    let second = writer.scan(&request).unwrap();
    assert_eq!(second.rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["b"]);
    assert_eq!(second.rows[0].contents[0].bytes.as_deref(), Some(b"new request".as_slice()));
    assert_eq!(second.stats.content_values_reconstructed, 1);
    assert_eq!(second.stats.reconstructed_bytes, b"new request".len() as u64);
    assert_eq!(second.stats.duplicate_attr_occurrences, 1);
    assert_eq!(
        second.rows[0]
            .attrs
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("status", AttrValue::Str("new".into())),
            ("tag", AttrValue::Int(1)),
            ("tag", AttrValue::Int(2)),
        ]
    );

    // Newest-wins happens before filtering: the staged `new` b hides committed `old` b.
    let old = ScanRequest {
        predicates: vec![Predicate::Attr {
            name: "status".into(),
            op: Compare::Eq,
            value: AttrValue::Str("old".into()),
        }],
        ..ScanRequest::default()
    };
    assert!(writer.scan(&old).unwrap().rows.is_empty());
    assert_eq!(reader.scan(&old).unwrap().rows[0].id, "b", "reader remains on its manifest");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_examination_budget_returns_a_continuation_even_with_no_matches() {
    let dir = tmp("budget");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for i in 0..10 {
        store
            .put_body(&format!("r{i:02}"), b"body", vec![("n".into(), AttrValue::Int(i))])
            .unwrap();
    }

    let mut request = ScanRequest {
        limit: 1,
        max_examined: 3,
        predicates: vec![Predicate::Attr {
            name: "n".into(),
            op: Compare::Eq,
            value: AttrValue::Int(9),
        }],
        ..ScanRequest::default()
    };
    let mut examined = 0usize;
    loop {
        let page = store.scan(&request).unwrap();
        examined += page.stats.examined;
        if let Some(row) = page.rows.first() {
            assert_eq!(row.id, "r09");
            break;
        }
        request.cursor = page.next;
        assert!(request.cursor.is_some(), "a bounded empty page must remain continuable");
    }
    assert_eq!(examined, 10);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reconstruction_budget_pages_whole_rows_without_gaps() {
    let dir = tmp("content-budget");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for id in ["a", "b", "c"] {
        store.put_body(id, b"123456", vec![]).unwrap();
    }

    let mut request = ScanRequest {
        contents: vec![ContentSelect { name: "body".into(), mode: ContentMode::Bytes }],
        max_reconstructed_bytes: 10,
        ..ScanRequest::default()
    };
    let mut ids = Vec::new();
    let mut pages = 0;
    loop {
        let page = store.scan(&request).unwrap();
        pages += 1;
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.stats.reconstructed_bytes, 6);
        ids.push(page.rows[0].id.clone());
        let Some(next) = page.next else {
            assert!(!page.stats.reconstruction_budget_exhausted);
            break;
        };
        assert!(page.stats.reconstruction_budget_exhausted);
        assert_eq!(page.stats.examined, 2, "the deferred row was inspected but not consumed");
        request.cursor = Some(next);
    }
    assert_eq!(pages, 3);
    assert_eq!(ids, ["a", "b", "c"]);

    request.cursor = None;
    request.direction = Direction::Reverse;
    let mut reverse_ids = Vec::new();
    loop {
        let page = store.scan(&request).unwrap();
        reverse_ids.extend(page.rows.into_iter().map(|row| row.id));
        let Some(next) = page.next else { break };
        request.cursor = Some(next);
    }
    assert_eq!(reverse_ids, ["c", "b", "a"]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reconstruction_budget_admits_one_oversized_row_and_counts_all_selected_content() {
    let dir = tmp("oversized-content-budget");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for id in ["a", "b"] {
        store
            .put_record(
                id,
                &[
                    ContentSpans::new("request", vec![Span::Piece(b"123456")]),
                    ContentSpans::new("response", vec![Span::Piece(b"abcdef")]),
                ],
                vec![],
            )
            .unwrap();
    }
    let mut request = ScanRequest {
        contents: vec![
            ContentSelect { name: "request".into(), mode: ContentMode::Bytes },
            ContentSelect { name: "response".into(), mode: ContentMode::Bytes },
        ],
        max_reconstructed_bytes: 5,
        ..ScanRequest::default()
    };

    let first = store.scan(&request).unwrap();
    assert_eq!(first.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["a"]);
    assert_eq!(first.stats.content_values_reconstructed, 2);
    assert_eq!(first.stats.reconstructed_bytes, 12);
    assert!(first.stats.reconstruction_budget_exhausted);

    request.cursor = first.next;
    let second = store.scan(&request).unwrap();
    assert_eq!(second.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["b"]);
    assert_eq!(second.stats.reconstructed_bytes, 12);
    assert!(!second.stats.reconstruction_budget_exhausted);
    assert!(second.next.is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn metadata_only_projection_does_not_spend_the_reconstruction_budget() {
    let dir = tmp("metadata-budget");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for id in ["a", "b", "c"] {
        store.put_body(id, b"far larger than one byte", vec![]).unwrap();
    }
    let page = store
        .scan(&ScanRequest {
            contents: vec![ContentSelect { name: "body".into(), mode: ContentMode::Metadata }],
            max_reconstructed_bytes: 1,
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(page.rows.len(), 3);
    assert_eq!(page.stats.content_values_reconstructed, 0);
    assert_eq!(page.stats.reconstructed_bytes, 0);
    assert!(!page.stats.reconstruction_budget_exhausted);
    assert!(page.next.is_none());
    assert!(store
        .scan(&ScanRequest { max_reconstructed_bytes: 0, ..ScanRequest::default() })
        .unwrap_err()
        .to_string()
        .contains("must be greater than zero"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cursor_tampering_or_request_reuse_is_refused() {
    let dir = tmp("cursor");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for id in ["a", "b", "c"] {
        store.put_body(id, b"body", status("x")).unwrap();
    }
    let request = ScanRequest { limit: 1, ..ScanRequest::default() };
    let cursor = store.scan(&request).unwrap().next.unwrap();

    let mut changed = request.clone();
    changed.cursor = Some(cursor.clone());
    changed.predicates.push(Predicate::ContentExists { name: "body".into(), present: true });
    assert!(
        store.scan(&changed).unwrap_err().downcast_ref::<ScanInputError>().is_some(),
        "a cursor cannot skip into a different predicate set"
    );

    let mut reversed = request.clone();
    reversed.cursor = Some(cursor.clone());
    reversed.direction = Direction::Reverse;
    assert!(store.scan(&reversed).unwrap_err().downcast_ref::<ScanInputError>().is_some());

    let mut damaged_token = cursor;
    let replacement = if damaged_token.ends_with('0') { "1" } else { "0" };
    damaged_token.replace_range(damaged_token.len() - 1.., replacement);
    let mut damaged = request;
    damaged.cursor = Some(damaged_token);
    assert!(
        store.scan(&damaged).unwrap_err().downcast_ref::<ScanInputError>().is_some(),
        "cursor checksum must catch tampering"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reverse_pages_are_complete_and_duplicate_free() {
    let dir = tmp("reverse");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for id in ["a", "b", "c", "d", "e"] {
        store.put_body(id, b"body", vec![]).unwrap();
    }
    let mut request =
        ScanRequest { direction: Direction::Reverse, limit: 2, ..ScanRequest::default() };
    let mut got = Vec::new();
    loop {
        let page = store.scan(&request).unwrap();
        got.extend(page.rows.into_iter().map(|row| row.id));
        let Some(next) = page.next else { break };
        request.cursor = Some(next);
    }
    assert_eq!(got, ["e", "d", "c", "b", "a"]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn bounded_part_walk_matches_the_materialized_visibility_oracle() {
    let dir = tmp("id-walk-oracle");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for generation in 0..5 {
        for i in 0..20 {
            if (i + generation) % 4 == 0 {
                store.delete(&format!("r{i:02}")).unwrap();
            } else {
                store
                    .put_body(
                        &format!("r{i:02}"),
                        format!("generation {generation}").as_bytes(),
                        vec![],
                    )
                    .unwrap();
            }
        }
        store.sync().unwrap();
        store.flush().unwrap();
    }
    let reader = turndb::store::open_read_container(&store_file(&dir), cfg()).unwrap();
    let all = reader.ids().unwrap();
    for (from, to) in
        [(None, None), (Some("r03"), None), (None, Some("r17")), (Some("r05"), Some("r14"))]
    {
        for reverse in [false, true] {
            for limit in [1, 2, 7, 100] {
                let mut want: Vec<String> = all
                    .iter()
                    .filter(|id| from.is_none_or(|bound| id.as_str() >= bound))
                    .filter(|id| to.is_none_or(|bound| id.as_str() < bound))
                    .cloned()
                    .collect();
                if reverse {
                    want.reverse();
                }
                want.truncate(limit);
                assert_eq!(
                    reader.scan_ids(from, to, limit, reverse).unwrap(),
                    want,
                    "from={from:?} to={to:?} reverse={reverse} limit={limit}"
                );
            }
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn resolved_candidates_project_and_reconstruct_the_authoritative_part_rows() {
    let dir = tmp("resolved-rows");
    {
        let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
        store.put_body("a", b"a0", vec![("generation".into(), AttrValue::Int(0))]).unwrap();
        store.put_body("b", b"b0", vec![("generation".into(), AttrValue::Int(0))]).unwrap();
        store.sync().unwrap();
        store.flush().unwrap();

        store.delete("a").unwrap();
        store.put_body("b", b"b1", vec![("generation".into(), AttrValue::Int(1))]).unwrap();
        store.put_body("c", b"c1", vec![("generation".into(), AttrValue::Int(1))]).unwrap();
        store.sync().unwrap();
        store.flush().unwrap();

        store.put_body("a", b"a2", vec![("generation".into(), AttrValue::Int(2))]).unwrap();
        store.delete("c").unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
    }

    let reader = turndb::store::open_read_container(&store_file(&dir), cfg()).unwrap();
    let page = reader
        .scan(&ScanRequest {
            attrs: vec!["generation".into()],
            contents: vec![ContentSelect { name: "body".into(), mode: ContentMode::Bytes }],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(page.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(page.rows[0].attrs, [("generation".into(), AttrValue::Int(2))]);
    assert_eq!(page.rows[0].contents[0].bytes.as_deref(), Some(b"a2".as_slice()));
    assert_eq!(page.rows[1].attrs, [("generation".into(), AttrValue::Int(1))]);
    assert_eq!(page.rows[1].contents[0].bytes.as_deref(), Some(b"b1".as_slice()));
    assert_eq!(page.stats.resolution.physical_rows, 7);
    assert_eq!(page.stats.resolution.superseded_rows, 4);
    assert_eq!(page.stats.resolution.tombstones, 1);
    assert_eq!(page.stats.resolution.memtable_entries, 0);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn resolution_budget_advances_across_tombstone_only_groups_in_both_directions() {
    let dir = tmp("resolution-budget");
    {
        let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
        for id in ["a", "b", "c", "d"] {
            store.put_body(id, id.as_bytes(), vec![]).unwrap();
        }
        store.sync().unwrap();
        store.flush().unwrap();

        store.delete("a").unwrap();
        store.delete("b").unwrap();
        store.put_body("c", b"c1", vec![]).unwrap();
        store.sync().unwrap();
        store.flush().unwrap();

        store.delete("c").unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
    }

    let reader = turndb::store::open_read_container(&store_file(&dir), cfg()).unwrap();
    for direction in [Direction::Forward, Direction::Reverse] {
        let mut request =
            ScanRequest { direction, max_resolution_entries: 2, ..ScanRequest::default() };
        let mut rows = Vec::new();
        let mut physical = Vec::new();
        let mut empty_pages = 0;
        for _ in 0..8 {
            let page = reader.scan(&request).unwrap();
            physical.push(page.stats.resolution.physical_rows);
            empty_pages += usize::from(page.rows.is_empty());
            rows.extend(page.rows.into_iter().map(|row| row.id));
            let Some(next) = page.next else { break };
            assert!(page.stats.resolution.budget_exhausted);
            request.cursor = Some(next);
        }
        assert_eq!(rows, ["d"]);
        assert!(empty_pages >= 3, "tombstone-only progress must be representable");
        assert!(
            physical.contains(&3),
            "one three-version id group must be admitted whole despite a budget of two"
        );
    }
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn resolution_budget_counts_the_writer_overlay_in_the_same_id_group() {
    let dir = tmp("resolution-overlay");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    store.put_body("a", b"committed-a", vec![]).unwrap();
    store.put_body("b", b"committed-b", vec![]).unwrap();
    store.sync().unwrap();
    store.flush().unwrap();
    store.delete("a").unwrap();
    store.put_body("c", b"staged-c", vec![]).unwrap();

    let mut request = ScanRequest { max_resolution_entries: 1, ..ScanRequest::default() };
    let first = store.scan(&request).unwrap();
    assert!(first.rows.is_empty());
    assert_eq!(first.stats.resolution.physical_rows, 1);
    assert_eq!(first.stats.resolution.memtable_entries, 1);
    assert_eq!(first.stats.resolution.superseded_rows, 1);
    assert!(first.stats.resolution.budget_exhausted);

    request.cursor = first.next;
    let second = store.scan(&request).unwrap();
    assert_eq!(second.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["b"]);
    assert!(second.stats.resolution.budget_exhausted);

    request.cursor = second.next;
    let third = store.scan(&request).unwrap();
    assert_eq!(third.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["c"]);
    assert!(!third.stats.resolution.budget_exhausted);
    assert!(third.next.is_none());
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn invalid_resolution_budgets_are_refused_before_range_work() {
    let dir = tmp("resolution-budget-validation");
    let store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for value in [0, turndb::scan::MAX_RESOLUTION_ENTRIES + 1] {
        assert!(store
            .scan(&ScanRequest { max_resolution_entries: value, ..ScanRequest::default() })
            .unwrap_err()
            .downcast_ref::<ScanInputError>()
            .is_some());
    }
    std::fs::remove_dir_all(dir).ok();
}

/// Both sides of the `limit` and `max_examined` ceilings. Refusal alone is half a test: a
/// validator that also refuses the extremes it must accept passes every rejects-bad-input
/// assertion, so the nearest VALID values — 1 and the exact maximum — must each return the
/// complete correct page.
#[test]
fn limit_and_examination_ceilings_refuse_extremes_and_accept_the_nearest_valid() {
    let dir = tmp("ceiling-validation");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for id in ["a", "b", "c"] {
        store.put_body(id, b"", vec![("x".into(), AttrValue::Int(1))]).unwrap();
    }
    store.sync().unwrap();
    store.flush().unwrap();

    for request in [
        ScanRequest { limit: 0, ..ScanRequest::default() },
        ScanRequest { limit: turndb::scan::MAX_LIMIT + 1, ..ScanRequest::default() },
        ScanRequest { max_examined: 0, ..ScanRequest::default() },
        ScanRequest { max_examined: turndb::scan::MAX_EXAMINED + 1, ..ScanRequest::default() },
    ] {
        assert!(store.scan(&request).unwrap_err().downcast_ref::<ScanInputError>().is_some());
    }

    for request in [
        ScanRequest { limit: turndb::scan::MAX_LIMIT, ..ScanRequest::default() },
        ScanRequest { max_examined: turndb::scan::MAX_EXAMINED, ..ScanRequest::default() },
        ScanRequest { limit: 1, max_examined: 1, ..ScanRequest::default() },
    ] {
        let complete = request.limit >= 3;
        let page = store.scan(&request).unwrap();
        if complete {
            assert_eq!(
                page.rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
                ["a", "b", "c"]
            );
            assert!(page.next.is_none(), "a complete page at a maximum ceiling carries no cursor");
        } else {
            assert_eq!(page.rows[0].id, "a");
            assert!(page.next.is_some(), "limit 1 of 3 must offer a continuation");
        }
    }
    std::fs::remove_dir_all(dir).ok();
}

/// The nearest valid deadline: every deadline test elsewhere uses an already-expired instant, and
/// an implementation that refused every deadline-bearing request would pass all of them. A
/// generous deadline and a live, uncancelled token must admit a complete page.
#[test]
fn a_generous_deadline_and_an_uncancelled_token_admit_a_complete_page() {
    let dir = tmp("generous-deadline");
    let mut store = Store::open_file(&store_file(&dir), cfg()).unwrap();
    for id in ["a", "b", "c"] {
        store.put_body(id, b"payload long enough to fold", vec![]).unwrap();
    }
    store.sync().unwrap();
    store.flush().unwrap();

    let token = CancellationToken::new();
    let page = store
        .scan(&ScanRequest {
            contents: vec![ContentSelect { name: "body".into(), mode: ContentMode::Bytes }],
            deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(3600)),
            cancellation: Some(token),
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(page.rows.len(), 3);
    assert!(page.rows.iter().all(|row| row.contents[0].bytes.is_some()));
    assert!(page.next.is_none(), "the deadline must not shorten a page it never reached");
    std::fs::remove_dir_all(dir).ok();
}

/// Build the suite's single-file store inside its cleanup directory.
fn store_file(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).ok();
    dir.join("s.turndb")
}
