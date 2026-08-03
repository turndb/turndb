use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::scan::{
    CancellationToken, Compare, ContentMode, ContentSelect, Direction, Predicate, ScanInterrupted,
    ScanInterruptionReason, ScanRequest,
};
use turndb::store::{ContentSpans, Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-scan-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    FoldCfg { seg_max: 1 << 22, ..FoldCfg::default() }
}

fn status(value: &str) -> Vec<(String, AttrValue)> {
    vec![("status".into(), AttrValue::Str(value.into()))]
}

#[test]
fn cancellation_and_deadlines_are_typed_and_return_no_partial_page() {
    let dir = tmp("interruption");
    let store = Store::open(&dir, cfg()).unwrap();

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
    let mut writer = Store::open(&dir, cfg()).unwrap();
    writer.put_body("b", b"old body", status("old")).unwrap();
    writer.sync().unwrap();
    writer.flush().unwrap();
    let reader = Store::open_read(&dir, cfg()).unwrap();

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
    assert_eq!(second.stats.shadowed_attr_occurrences, 1);
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
    let mut store = Store::open(&dir, cfg()).unwrap();
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
    let mut store = Store::open(&dir, cfg()).unwrap();
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
    let mut store = Store::open(&dir, cfg()).unwrap();
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
    let mut store = Store::open(&dir, cfg()).unwrap();
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
    let mut store = Store::open(&dir, cfg()).unwrap();
    for id in ["a", "b", "c"] {
        store.put_body(id, b"body", status("x")).unwrap();
    }
    let request = ScanRequest { limit: 1, ..ScanRequest::default() };
    let cursor = store.scan(&request).unwrap().next.unwrap();

    let mut changed = request.clone();
    changed.cursor = Some(cursor.clone());
    changed.predicates.push(Predicate::ContentExists { name: "body".into(), present: true });
    assert!(store.scan(&changed).is_err(), "a cursor cannot skip into a different predicate set");

    let mut reversed = request.clone();
    reversed.cursor = Some(cursor.clone());
    reversed.direction = Direction::Reverse;
    assert!(store.scan(&reversed).is_err());

    let mut damaged_token = cursor;
    let replacement = if damaged_token.ends_with('0') { "1" } else { "0" };
    damaged_token.replace_range(damaged_token.len() - 1.., replacement);
    let mut damaged = request;
    damaged.cursor = Some(damaged_token);
    assert!(store.scan(&damaged).is_err(), "cursor checksum must catch tampering");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reverse_pages_are_complete_and_duplicate_free() {
    let dir = tmp("reverse");
    let mut store = Store::open(&dir, cfg()).unwrap();
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
    let mut store = Store::open(&dir, cfg()).unwrap();
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
    let reader = Store::open_read(&dir, cfg()).unwrap();
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
