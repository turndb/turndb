//! The query lens: columnar reads, projection, and SQL over a real store.
//!
//! The load-bearing test here is `columnar_and_row_paths_agree_exactly`. Two independent decoders now
//! read the same bytes — the row API walks the layout, the lens scatters columns — and a divergence
//! between them is a silent wrong answer, the worst failure this system can have.

#![cfg(feature = "sql")]

use datafusion::arrow::array::{Array, AsArray};
use datafusion::catalog::TableProvider;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use turndb::fold::FoldCfg;
use turndb::part::Part;
use turndb::query::{
    collect,
    sql::{classify_error, SqlBudget, SqlErrorClass, SqlOptions, SqlQuery, SqlValue},
    table::TurndbTable,
    Lens,
};
use turndb::store::{ContentSpans, Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-query-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    FoldCfg { seg_max: 1 << 22, ..FoldCfg::default() }
}

/// A store of `n` records per flush over `flushes` flushes, with a realistic attribute mix.
fn build(dir: &Path, flushes: usize, per: usize) -> Vec<(String, Vec<u8>)> {
    let mut s = Store::open(dir, cfg()).unwrap();
    let mut want = Vec::new();
    for f in 0..flushes {
        for i in 0..per {
            let id = format!("t{f:02}-{i:04}");
            let body =
                format!("body of {id}, with enough text to be worth folding at all").into_bytes();
            let attrs = vec![
                (
                    "model".to_string(),
                    AttrValue::Str(if i % 3 == 0 { "opus" } else { "sonnet" }.into()),
                ),
                ("tokens".to_string(), AttrValue::Int((i * 10) as i64)),
                ("cost".to_string(), AttrValue::Float(i as f64 * 0.001)),
                ("ok".to_string(), AttrValue::Bool(i % 5 != 0)),
            ];
            s.put(&id, &[Span::Piece(&body)], attrs).unwrap();
            want.push((id, body));
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    want
}

fn parts_of(dir: &PathBuf) -> Vec<Arc<Part>> {
    let mut ps: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .collect();
    ps.sort();
    ps.iter().map(|p| Arc::new(Part::open(p).unwrap())).collect()
}

// ---------------------------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------------------------

#[test]
fn schema_names_columns_by_key_and_never_merges_two_types() {
    let dir = tmp("schema");
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.put("a", &[Span::Lit(b"x")], vec![("v".into(), AttrValue::Int(1))]).unwrap();
    // Same key, different type — these are two homogeneous columns, and must stay two fields.
    s.put("b", &[Span::Lit(b"y")], vec![("v".into(), AttrValue::Str("one".into()))]).unwrap();
    s.put("c", &[Span::Lit(b"z")], vec![("only".into(), AttrValue::Bool(true))]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();

    let lens = Lens::new(&parts_of(&dir)).unwrap();
    let names: Vec<String> = lens.schema().fields().iter().map(|f| f.name().clone()).collect();
    // Split fields order by type tag (str=0, int=1), which is stable across runs and across parts.
    assert_eq!(
        names,
        vec!["id", "body", "only", "v#str", "v#int"],
        "a single-typed key keeps its name; a multi-typed key is split, never merged"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn extended_scalar_columns_keep_exact_arrow_types_and_null_presence() {
    use datafusion::arrow::datatypes::{DataType, TimeUnit};

    let dir = tmp("extended-scalars");
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.put(
        "a",
        &[Span::Lit(b"")],
        vec![
            ("u".into(), AttrValue::UInt(u64::MAX)),
            ("raw".into(), AttrValue::Bytes(vec![0, 0xff, 1])),
            ("at".into(), AttrValue::TimestampNs(-1_234_567_890)),
            ("nothing".into(), AttrValue::Null),
        ],
    )
    .unwrap();
    s.put(
        "b",
        &[Span::Lit(b"")],
        vec![
            ("u".into(), AttrValue::UInt(7)),
            ("raw".into(), AttrValue::Bytes(vec![0, 1])),
            ("at".into(), AttrValue::TimestampNs(i64::MAX)),
        ],
    )
    .unwrap();
    s.sync().unwrap();
    s.flush().unwrap();

    let parts = parts_of(&dir);
    let lens = Lens::new(&parts).unwrap();
    let schema = lens.schema();
    assert_eq!(schema.field_with_name("u").unwrap().data_type(), &DataType::UInt64);
    assert_eq!(
        schema.field_with_name("raw").unwrap().data_type(),
        &DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Binary))
    );
    assert_eq!(
        schema.field_with_name("at").unwrap().data_type(),
        &DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()))
    );
    assert_eq!(schema.field_with_name("nothing#null").unwrap().data_type(), &DataType::Boolean);

    let projection = lens.project(&["id", "u", "raw", "at", "nothing#null"]).unwrap();
    let (batches, _) = collect(&parts, None, &lens, &projection).unwrap();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(batch.num_rows(), 2);
    let null_presence = batch.column(4).as_boolean();
    assert!(null_presence.value(0));
    assert!(null_presence.is_null(1), "missing and explicit null must remain distinct");

    for id in ["a", "b"] {
        let row = s.get(id).unwrap().unwrap();
        assert_eq!(
            row.attrs,
            if id == "a" {
                vec![
                    ("u".into(), AttrValue::UInt(u64::MAX)),
                    ("raw".into(), AttrValue::Bytes(vec![0, 0xff, 1])),
                    ("at".into(), AttrValue::TimestampNs(-1_234_567_890)),
                    ("nothing".into(), AttrValue::Null),
                ]
            } else {
                vec![
                    ("u".into(), AttrValue::UInt(7)),
                    ("raw".into(), AttrValue::Bytes(vec![0, 1])),
                    ("at".into(), AttrValue::TimestampNs(i64::MAX)),
                ]
            }
        );
    }
    let reader = Store::open_read(&dir, cfg()).unwrap();
    let mut query = SqlQuery::open(
        reader,
        "SELECT id FROM records WHERE u = $1 AND raw = $2 AND at = $3",
        vec![
            SqlValue::UInt(u64::MAX),
            SqlValue::Binary(vec![0, 0xff, 1]),
            SqlValue::TimestampNs(-1_234_567_890),
        ],
        SqlOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(query.next().await.unwrap().unwrap().rows, 1);
    assert!(query.next().await.unwrap().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn parts_with_different_columns_share_one_row_shape() {
    let dir = tmp("union");
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.put("a", &[Span::Lit(b"x")], vec![("early".into(), AttrValue::Int(1))]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    // A column that did not exist when the first part was written.
    s.put("b", &[Span::Lit(b"y")], vec![("late".into(), AttrValue::Int(2))]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();

    let parts = parts_of(&dir);
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["id", "early", "late"]).unwrap();
    let (batches, _) = collect(&parts, None, &lens, &proj).unwrap();

    let mut seen = Vec::new();
    for b in &batches {
        let ids = b.column(0).as_string::<i32>();
        let early = b.column(1).as_primitive::<datafusion::arrow::datatypes::Int64Type>();
        let late = b.column(2).as_primitive::<datafusion::arrow::datatypes::Int64Type>();
        for r in 0..b.num_rows() {
            seen.push((
                ids.value(r).to_string(),
                early.is_valid(r).then(|| early.value(r)),
                late.is_valid(r).then(|| late.value(r)),
            ));
        }
    }
    assert_eq!(
        seen,
        vec![("a".to_string(), Some(1), None), ("b".to_string(), None, Some(2)),],
        "a part lacking a column contributes nulls, not a schema mismatch"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Projection — the reason this layer exists
// ---------------------------------------------------------------------------------------------

#[test]
fn an_attribute_scan_never_opens_the_fold() {
    let dir = tmp("proj");
    build(&dir, 3, 200);
    let parts = parts_of(&dir);
    let lens = Lens::new(&parts).unwrap();

    let proj = lens.project(&["model", "tokens"]).unwrap();
    // `None` for the fold: not merely unused, but unavailable. A scan that tried would fail loudly.
    let (batches, stats) = collect(&parts, None, &lens, &proj).unwrap();
    assert_eq!(stats.rows, 600);
    assert_eq!(stats.fold_reads, 0, "AN ATTRIBUTE-ONLY SCAN MUST NOT TOUCH CONTENT");
    assert_eq!(stats.columns_decoded, 6, "two columns per part, three parts — not all four");
    assert!(
        batches.iter().all(|b| b.num_columns() == 2),
        "only projected columns are materialised"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn projecting_body_without_a_fold_is_refused_rather_than_wrong() {
    let dir = tmp("nofold");
    build(&dir, 1, 10);
    let parts = parts_of(&dir);
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["id", "body"]).unwrap();
    assert!(
        collect(&parts, None, &lens, &proj).is_err(),
        "asking for content with no fold must fail, never return nulls"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// The differential gate
// ---------------------------------------------------------------------------------------------

#[test]
fn columnar_and_row_paths_agree_exactly() {
    let dir = tmp("differential");
    let want = build(&dir, 4, 250);
    let parts = parts_of(&dir);
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["id", "model", "tokens", "cost", "ok"]).unwrap();
    let (batches, stats) = collect(&parts, None, &lens, &proj).unwrap();
    assert_eq!(stats.rows, 1000);
    assert_eq!(stats.shadowed_occurrences, 0, "this corpus has no repeated keys to shadow");

    // Flatten the columnar read into rows...
    let mut got = Vec::new();
    for b in &batches {
        let ids = b.column(0).as_string::<i32>();
        let model = b.column(1).as_dictionary::<datafusion::arrow::datatypes::Int32Type>();
        let mvals = model.values().as_string::<i32>();
        let tok = b.column(2).as_primitive::<datafusion::arrow::datatypes::Int64Type>();
        let cost = b.column(3).as_primitive::<datafusion::arrow::datatypes::Float64Type>();
        let ok = b.column(4).as_boolean();
        for r in 0..b.num_rows() {
            got.push((
                ids.value(r).to_string(),
                mvals.value(model.key(r).unwrap()).to_string(),
                tok.value(r),
                cost.value(r),
                ok.value(r),
            ));
        }
    }

    // ...and compare against the independent row decoder, record by record.
    let mut expect = Vec::new();
    for p in &parts {
        for r in 0..p.len() {
            let rec = p.record(r).unwrap();
            let get = |k: &str| rec.attrs.iter().find(|(a, _)| a == k).unwrap().1.clone();
            let s = match get("model") {
                AttrValue::Str(s) => s,
                v => panic!("model was {v:?}"),
            };
            let t = match get("tokens") {
                AttrValue::Int(i) => i,
                v => panic!("tokens was {v:?}"),
            };
            let c = match get("cost") {
                AttrValue::Float(f) => f,
                v => panic!("cost was {v:?}"),
            };
            let o = match get("ok") {
                AttrValue::Bool(b) => b,
                v => panic!("ok was {v:?}"),
            };
            expect.push((rec.id, s, t, c, o));
        }
    }
    assert_eq!(got.len(), expect.len());
    assert_eq!(
        got, expect,
        "THE COLUMNAR AND ROW DECODERS DISAGREE — one of them is silently wrong"
    );

    // And the body column must reconstruct byte-exactly, same as the row path.
    let store = Store::open_read(&dir, cfg()).unwrap();
    for (id, body) in want.iter().take(50) {
        assert_eq!(&store.reconstruct(id).unwrap().unwrap(), body);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_body_column_is_byte_exact() {
    let dir = tmp("bodycol");
    let want = build(&dir, 2, 100);
    let store = Store::open_read(&dir, cfg()).unwrap();
    let (fold, parts) = store.into_parts();
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["id", "body"]).unwrap();
    let (batches, stats) = collect(&parts, Some(&fold), &lens, &proj).unwrap();
    assert_eq!(stats.fold_reads, 200, "every row's body came out of the fold");

    let mut got = std::collections::HashMap::new();
    for b in &batches {
        let ids = b.column(0).as_string::<i32>();
        let bodies = b.column(1).as_binary::<i32>();
        for r in 0..b.num_rows() {
            got.insert(ids.value(r).to_string(), bodies.value(r).to_vec());
        }
    }
    for (id, body) in &want {
        assert_eq!(got.get(id), Some(body), "body column diverged from what was written for {id}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_repeated_key_surfaces_its_first_value_and_counts_the_rest() {
    let dir = tmp("shadow");
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.put(
        "r",
        &[Span::Lit(b"x")],
        vec![
            ("tag".into(), AttrValue::Str("first".into())),
            ("tag".into(), AttrValue::Str("second".into())),
            ("tag".into(), AttrValue::Str("third".into())),
        ],
    )
    .unwrap();
    s.sync().unwrap();
    s.flush().unwrap();

    let parts = parts_of(&dir);
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["tag"]).unwrap();
    let (batches, stats) = collect(&parts, None, &lens, &proj).unwrap();
    let d = batches[0].column(0).as_dictionary::<datafusion::arrow::datatypes::Int32Type>();
    let v = d.values().as_string::<i32>();
    assert_eq!(v.value(d.key(0).unwrap()), "first", "the flat view takes the first occurrence");
    assert_eq!(
        stats.shadowed_occurrences, 2,
        "the other two must be COUNTED, not silently dropped"
    );

    // and the substrate still holds all three, in order
    let rec = parts[0].record(0).unwrap();
    assert_eq!(
        rec.attrs.len(),
        3,
        "the row path is lossless regardless of what the flat view shows"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn sql_selects_filters_and_aggregates() {
    let dir = tmp("sql");
    build(&dir, 3, 300);
    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, _table) = TurndbTable::context(store, "traces").unwrap();

    // count(*) projects ZERO columns — the batch must still carry its row count.
    let n = ctx.sql("SELECT count(*) AS n FROM traces").await.unwrap().collect().await.unwrap();
    assert_eq!(
        n[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0),
        900
    );

    let r = ctx
        .sql("SELECT model, count(*) AS n, sum(tokens) AS t FROM traces GROUP BY model ORDER BY model")
        .await.unwrap().collect().await.unwrap();
    let b = &r[0];
    let m = b.column(0).as_dictionary::<datafusion::arrow::datatypes::Int32Type>();
    let mv = m.values().as_string::<i32>();
    let cnt = b.column(1).as_primitive::<datafusion::arrow::datatypes::Int64Type>();
    let rows: Vec<(String, i64)> = (0..b.num_rows())
        .map(|i| (mv.value(m.key(i).unwrap()).to_string(), cnt.value(i)))
        .collect();
    assert_eq!(
        rows,
        vec![("opus".to_string(), 300), ("sonnet".to_string(), 600)],
        "100 of every 300 rows are opus, across 3 parts"
    );

    let f = ctx
        .sql("SELECT id FROM traces WHERE model = 'opus' AND tokens > 2900 ORDER BY id")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let total: usize = f.iter().map(|b| b.num_rows()).sum();
    assert!(total > 0, "the filter must match something");
    for b in &f {
        let ids = b.column(0).as_string::<i32>();
        for i in 0..b.num_rows() {
            let n: usize = ids.value(i).split('-').nth(1).unwrap().parse().unwrap();
            assert!(
                n.is_multiple_of(3) && n * 10 > 2900,
                "row {} does not satisfy the predicate",
                ids.value(i)
            );
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn embedded_sql_is_read_only_parameterized_bounded_and_arrow_streamed() {
    use datafusion::arrow::ipc::reader::StreamReader;
    use std::io::Cursor;

    let dir = tmp("embedded-sql");
    let mut writer = Store::open(&dir, cfg()).unwrap();
    for (id, kind, tokens) in [("a", "keep", 1), ("b", "drop", 2), ("c", "keep", 3)] {
        writer
            .put_body(
                id,
                b"payload",
                vec![
                    ("kind".into(), AttrValue::Str(kind.into())),
                    ("tokens".into(), AttrValue::Int(tokens)),
                ],
            )
            .unwrap();
    }
    writer.sync().unwrap();
    writer.flush().unwrap();
    let reader = Store::open_read(&dir, cfg()).unwrap();

    // ReadStore clones retain the same immutable fold and parts rather than reopening files or
    // moving the caller's snapshot into one query.
    let mut query = SqlQuery::open(
        reader.clone(),
        "SELECT id, tokens FROM records WHERE kind = $1 AND tokens > $2 ORDER BY id",
        vec![SqlValue::String("keep".into()), SqlValue::Int(1)],
        SqlOptions { max_memory_bytes: 32 << 20 },
    )
    .await
    .unwrap();
    let schema = StreamReader::try_new(Cursor::new(query.schema_ipc()), None).unwrap();
    assert_eq!(schema.schema().fields().len(), 2);
    assert_eq!(schema.count(), 0, "schema IPC contains no invented result row");

    let mut ids = Vec::new();
    while let Some(batch) = query.next().await.unwrap() {
        assert_eq!(batch.rows, 1);
        let decoded = StreamReader::try_new(Cursor::new(batch.ipc), None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(decoded.len(), 1, "each pull is one independently decodable IPC batch");
        let column = decoded[0].column(0).as_string::<i32>();
        ids.extend((0..decoded[0].num_rows()).map(|row| column.value(row).to_string()));
    }
    assert_eq!(ids, ["c"]);
    assert!(query.is_finished());
    assert!(query.next().await.unwrap().is_none(), "end of stream is stable");
    assert!(query.stats().rows > 0);

    let mut starved = SqlQuery::open(
        reader.clone(),
        "SELECT id FROM records ORDER BY id",
        vec![],
        SqlOptions { max_memory_bytes: 1 << 20 },
    )
    .await
    .unwrap();
    let error = starved.next().await.unwrap_err();
    assert_eq!(classify_error(&error), SqlErrorClass::ResourceExhausted);
    assert!(
        error.to_string().contains("execute TurnDB SQL batch"),
        "the configured execution pool must bound work rather than be advisory: {error:#}"
    );

    let error = SqlQuery::open(
        reader.clone(),
        "CREATE TABLE forbidden (value INT)",
        vec![],
        SqlOptions::default(),
    )
    .await
    .err()
    .expect("DDL must be rejected");
    assert_eq!(classify_error(&error), SqlErrorClass::InvalidArgument);
    assert!(error.to_string().contains("read-only"));

    let error = SqlQuery::open(
        reader,
        "SELECT id FROM records",
        vec![],
        SqlOptions { max_memory_bytes: 0 },
    )
    .await
    .err()
    .expect("zero memory must be rejected");
    assert!(error.to_string().contains("must be greater than zero"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn shared_sql_budget_bounds_concurrent_query_ceilings_and_releases_promptly() {
    let dir = tmp("sql-aggregate-budget");
    build(&dir, 1, 3);
    let reader = Store::open_read(&dir, cfg()).unwrap();
    let budget = SqlBudget::new(48 << 20).unwrap();
    let options = SqlOptions { max_memory_bytes: 32 << 20 };

    let mut first = SqlQuery::open_with_budget(
        reader.clone(),
        "SELECT id FROM records",
        vec![],
        options,
        &budget,
    )
    .await
    .unwrap();
    assert_eq!(budget.reserved(), 32 << 20);
    let error = SqlQuery::open_with_budget(
        reader.clone(),
        "SELECT id FROM records",
        vec![],
        options,
        &budget,
    )
    .await
    .err()
    .expect("the second ceiling exceeds the remaining aggregate budget");
    assert_eq!(classify_error(&error), SqlErrorClass::ResourceExhausted);
    assert_eq!(budget.reserved(), 32 << 20, "a failed reservation must consume nothing");

    while first.next().await.unwrap().is_some() {}
    assert_eq!(budget.reserved(), 0, "EOF releases before the query handle is dropped");
    let second =
        SqlQuery::open_with_budget(reader, "SELECT id FROM records", vec![], options, &budget)
            .await
            .unwrap();
    assert_eq!(budget.reserved(), 32 << 20);
    drop(second);
    assert_eq!(budget.reserved(), 0, "drop/cancellation releases the reservation");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn sql_over_attributes_reads_no_content() {
    let dir = tmp("sqlproj");
    build(&dir, 3, 300);
    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, table) = TurndbTable::context(store, "traces").unwrap();
    table.reset_stats();

    ctx.sql("SELECT model, avg(cost) FROM traces GROUP BY model")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    let st = table.stats();
    assert_eq!(st.fold_reads, 0,
        "an aggregate over attributes reconstructed {} bodies; projection pushdown is not reaching the scan",
        st.fold_reads);
    assert_eq!(st.rows, 900);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn sql_can_still_reach_content_when_it_asks_for_it() {
    let dir = tmp("sqlbody");
    let want = build(&dir, 1, 40);
    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, table) = TurndbTable::context(store, "traces").unwrap();

    let r = ctx
        .sql("SELECT id, body FROM traces WHERE id = 't00-0007'")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let b = r.iter().find(|b| b.num_rows() > 0).expect("the row exists");
    let body = b.column(1).as_binary::<i32>().value(0);
    let expect = &want.iter().find(|(i, _)| i == "t00-0007").unwrap().1;
    assert_eq!(body, expect.as_slice(), "SQL must return content byte-exactly");
    assert!(table.stats().fold_reads > 0, "this query genuinely did read the fold");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn named_content_columns_are_sparse_independent_and_lazy() {
    let dir = tmp("named-content");
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.put_record(
        "a",
        &[ContentSpans::new("request", vec![Span::Piece(b"request-a")])],
        vec![("kind".into(), AttrValue::Str("one".into()))],
    )
    .unwrap();
    s.put_record(
        "b",
        &[ContentSpans::new("response", vec![Span::Piece(b"response-b")])],
        vec![("kind".into(), AttrValue::Str("two".into()))],
    )
    .unwrap();
    s.put_record(
        "c",
        &[
            ContentSpans::new("request", vec![Span::Piece(b"request-c")]),
            ContentSpans::new("response", vec![Span::Piece(b"response-c")]),
        ],
        vec![("kind".into(), AttrValue::Str("three".into()))],
    )
    .unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, table) = TurndbTable::context(store, "t").unwrap();
    let schema = table.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert!(names.contains(&"content.request"));
    assert!(names.contains(&"content.response"));
    assert!(!names.contains(&"body"), "body exists only when a record actually names it");

    table.reset_stats();
    let batches = ctx
        .sql("SELECT id, \"content.request\" FROM t ORDER BY id")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let batch = datafusion::arrow::compute::concat_batches(&batches[0].schema(), &batches).unwrap();
    let ids = batch.column(0).as_string::<i32>();
    let requests = batch.column(1).as_binary::<i32>();
    assert_eq!(ids.value(0), "a");
    assert_eq!(requests.value(0), b"request-a");
    assert!(requests.is_null(1), "a sparse content miss is NULL, not empty bytes");
    assert_eq!(requests.value(2), b"request-c");
    assert_eq!(
        table.stats().fold_reads,
        2,
        "projecting request reconstructs its two values and no response values"
    );

    table.reset_stats();
    ctx.sql("SELECT kind FROM t").await.unwrap().collect().await.unwrap();
    assert_eq!(table.stats().fold_reads, 0, "metadata projection cannot reach any content column");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn sql_exposes_one_live_version_and_never_falls_through_a_filter() {
    let dir = tmp("visibility");
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.put(
        "x",
        &[Span::Lit(b"old body")],
        vec![("kind".into(), AttrValue::Str("old-match".into()))],
    )
    .unwrap();
    s.put(
        "deleted",
        &[Span::Lit(b"must disappear")],
        vec![("kind".into(), AttrValue::Str("old-match".into()))],
    )
    .unwrap();
    s.sync().unwrap();
    s.flush().unwrap();

    s.put(
        "x",
        &[Span::Lit(b"new body")],
        vec![("kind".into(), AttrValue::Str("new-value".into()))],
    )
    .unwrap();
    s.delete("deleted").unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, table) = TurndbTable::context(store, "t").unwrap();

    let count = ctx.sql("SELECT count(*) FROM t").await.unwrap().collect().await.unwrap();
    assert_eq!(
        count[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0),
        1,
        "SQL must expose one live row, not physical versions or tombstones"
    );

    let rows = ctx.sql("SELECT id, body FROM t").await.unwrap().collect().await.unwrap();
    let batch = rows.iter().find(|b| b.num_rows() > 0).expect("x is live");
    assert_eq!(batch.column(0).as_string::<i32>().value(0), "x");
    assert_eq!(batch.column(1).as_binary::<i32>().value(0), b"new body");

    let old = ctx
        .sql("SELECT count(*) FROM t WHERE kind = 'old-match'")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(
        old[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0),
        0,
        "a newest version that fails a predicate must not reveal an older matching version"
    );
    assert!(
        table.stats().rows_hidden >= 3,
        "two superseded rows and one tombstone should be hidden"
    );

    // Neither ordinary compaction nor the re-folding GC may change the logical SQL answer.
    drop(ctx);
    drop(table);
    let mut s = Store::open(&dir, cfg()).unwrap();
    let n = s.part_count();
    s.merge_range(0, n).unwrap();
    s.refold().unwrap();
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, _) = TurndbTable::context(store, "t").unwrap();
    let count = ctx.sql("SELECT count(*) FROM t").await.unwrap().collect().await.unwrap();
    assert_eq!(
        count[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0),
        1,
        "compaction and refolding must preserve the logical SQL snapshot"
    );
    let old = ctx
        .sql("SELECT count(*) FROM t WHERE kind = 'old-match'")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(
        old[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0),
        0,
        "physical reclamation must not change predicate results"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_limit_counts_visible_rows_not_physical_prefix_rows() {
    let dir = tmp("visiblelimit");
    let mut s = Store::open(&dir, cfg()).unwrap();
    // In the older part, `a` is row zero and `z` is row one. A newer `a` hides row zero, so a
    // physical-prefix interpretation of fetch=1 would return nothing instead of the visible `z`.
    s.put("a", &[Span::Lit(b"old a")], vec![]).unwrap();
    s.put("z", &[Span::Lit(b"live z")], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    s.put("a", &[Span::Lit(b"new a")], vec![]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (fold, parts) = store.into_parts();
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["id"]).unwrap();
    let mut scan = lens.scan(&parts[0], Some(&fold), &proj, &[]).unwrap().with_fetch(Some(1));
    let batch = scan.next_batch().unwrap().expect("the older part still has one live row");
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(batch.column(0).as_string::<i32>().value(0), "z");
    assert!(scan.next_batch().unwrap().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_body_batch_is_bounded_by_bytes_not_row_count() {
    let dir = tmp("batchbytes");
    let mut s = Store::open(&dir, cfg()).unwrap();
    // 400 records of ~256 KiB each: far under BATCH_ROWS, far over BATCH_BYTES.
    let mut want = Vec::new();
    for i in 0..400u32 {
        let body: Vec<u8> = (0..8192u32)
            .flat_map(|j| blake3::hash(&(i * 100_000 + j).to_le_bytes()).as_bytes()[..32].to_vec())
            .collect();
        s.put(&format!("b{i:04}"), &[Span::Piece(&body)], vec![]).unwrap();
        want.push(body);
    }
    s.sync().unwrap();
    s.flush().unwrap();

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (fold, parts) = store.into_parts();
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["id", "body"]).unwrap();

    let mut sc = lens.scan(&parts[0], Some(&fold), &proj, &[]).unwrap();
    let (mut batches, mut rows, mut peak) = (0usize, 0usize, 0usize);
    let mut seen: Vec<Vec<u8>> = Vec::new();
    while let Some(b) = sc.next_batch().unwrap() {
        let bytes: usize =
            (0..b.num_rows()).map(|r| b.column(1).as_binary::<i32>().value(r).len()).sum();
        peak = peak.max(bytes);
        rows += b.num_rows();
        batches += 1;
        for r in 0..b.num_rows() {
            seen.push(b.column(1).as_binary::<i32>().value(r).to_vec());
        }
    }
    assert_eq!(rows, 400, "every row must still be delivered");
    assert!(batches > 1, "400 huge records must not arrive as one batch (got {batches})");
    // One row always lands however big it is, so the ceiling can be exceeded by at most one record.
    assert!(
        peak <= turndb::query::BATCH_BYTES + 300_000,
        "a batch carried {peak} content bytes, past the ceiling"
    );
    assert_eq!(seen, want, "byte-bounded batching must not reorder or corrupt content");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_body_query_streams_instead_of_materialising_the_part() {
    let dir = tmp("sqlstream");
    // 300 records of ~256 KiB = ~75 MiB of content in ONE part. Eager materialisation would build all
    // of it in the partition before yielding a single row; streaming yields it a batch at a time.
    let mut s = Store::open(&dir, cfg()).unwrap();
    for i in 0..300u32 {
        let body: Vec<u8> = (0..8192u32)
            .flat_map(|j| blake3::hash(&(i * 77_777 + j).to_le_bytes()).as_bytes()[..32].to_vec())
            .collect();
        s.put(
            &format!("s{i:04}"),
            &[Span::Piece(&body)],
            vec![("n".into(), AttrValue::Int(i as i64))],
        )
        .unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, table) = TurndbTable::context(store, "t").unwrap();

    // LIMIT is the observable proof of laziness: an eager plan reconstructs every body in the
    // partition regardless, a streaming one stops as soon as the consumer has enough.
    table.reset_stats();
    let r = ctx.sql("SELECT id, body FROM t LIMIT 5").await.unwrap().collect().await.unwrap();
    let got: usize = r.iter().map(|b| b.num_rows()).sum();
    assert_eq!(got, 5);
    let st = table.stats();
    assert!(
        st.fold_reads < 300,
        "LIMIT 5 reconstructed {} of 300 bodies — the scan is not lazy",
        st.fold_reads
    );
    assert!(st.fold_reads >= 5, "it must have read at least the rows it returned");
    assert!(
        st.fold_reads <= 20,
        "LIMIT 5 reconstructed {} bodies — the limit is not reaching the scan",
        st.fold_reads
    );

    // and a full body scan still returns everything, byte-exact
    table.reset_stats();
    let all = ctx.sql("SELECT id, body FROM t").await.unwrap().collect().await.unwrap();
    let rows: usize = all.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 300);
    assert!(all.len() > 1, "300 large records must arrive as several batches, not one");
    for b in &all {
        let ids = b.column(0).as_string::<i32>();
        let bodies = b.column(1).as_binary::<i32>();
        for r in 0..b.num_rows() {
            let i: u32 = ids.value(r).trim_start_matches('s').parse().unwrap();
            let want: Vec<u8> = (0..8192u32)
                .flat_map(|j| {
                    blake3::hash(&(i * 77_777 + j).to_le_bytes()).as_bytes()[..32].to_vec()
                })
                .collect();
            assert_eq!(
                bodies.value(r),
                want.as_slice(),
                "streamed body diverged for {}",
                ids.value(r)
            );
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn limit_cost_does_not_scale_with_part_count() {
    // Streaming bounds memory, not work. Every partition executes, so without limit pushdown a
    // `LIMIT 1` reconstructs one BATCH of bodies per part — on a 400-part store, thousands of
    // reconstructions to return one row.
    let dir = tmp("limitparts");
    let mut s = Store::open(&dir, cfg()).unwrap();
    for p in 0..8u32 {
        for i in 0..40u32 {
            let body: Vec<u8> = (0..2048u32)
                .flat_map(|j| {
                    blake3::hash(&(p * 1_000_000 + i * 1000 + j).to_le_bytes()).as_bytes()[..32]
                        .to_vec()
                })
                .collect();
            s.put(&format!("p{p}-{i:03}"), &[Span::Piece(&body)], vec![]).unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    assert_eq!(s.part_count(), 8);
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, table) = TurndbTable::context(store, "t").unwrap();
    table.reset_stats();
    let r = ctx.sql("SELECT id, body FROM t LIMIT 1").await.unwrap().collect().await.unwrap();
    assert_eq!(r.iter().map(|b| b.num_rows()).sum::<usize>(), 1);

    let st = table.stats();
    // 8 parts x 40 records = 320. Without pushdown this reconstructs all 320 (each part is one batch).
    assert!(
        st.fold_reads <= 8,
        "LIMIT 1 over 8 parts reconstructed {} bodies; cost is scaling with part count",
        st.fold_reads
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Filter pushdown. The point is not fewer rows out — the engine could do that. It is less WORK in.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_filtered_body_query_reconstructs_only_matching_rows() {
    let dir = tmp("filterbody");
    let mut s = Store::open(&dir, cfg()).unwrap();
    for i in 0..600u32 {
        let body: Vec<u8> = (0..512u32)
            .flat_map(|j| blake3::hash(&(i * 4096 + j).to_le_bytes()).as_bytes()[..32].to_vec())
            .collect();
        // exactly 1 in 30 is "rare"
        let kind = if i % 30 == 0 { "rare" } else { "common" };
        s.put(
            &format!("f{i:04}"),
            &[Span::Piece(&body)],
            vec![
                ("kind".into(), AttrValue::Str(kind.into())),
                ("n".into(), AttrValue::Int(i as i64)),
            ],
        )
        .unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, table) = TurndbTable::context(store, "t").unwrap();
    table.reset_stats();

    let r = ctx
        .sql("SELECT id, body FROM t WHERE kind = 'rare'")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let rows: usize = r.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 20, "600/30 rows match");

    let st = table.stats();
    // Without pushdown every one of the 600 bodies is reconstructed and the engine filters after.
    assert!(
        st.fold_reads <= 40,
        "reconstructed {} bodies to return 20 — the predicate is not reaching the scan",
        st.fold_reads
    );
    assert!(st.rows_filtered >= 500, "the scan should have excluded most rows itself");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_predicate_matching_nothing_skips_batches_whole() {
    let dir = tmp("filternone");
    let mut s = Store::open(&dir, cfg()).unwrap();
    for i in 0..500u32 {
        s.put(
            &format!("z{i:04}"),
            &[Span::Lit(b"x")],
            vec![("kind".into(), AttrValue::Str("present".into()))],
        )
        .unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, table) = TurndbTable::context(store, "t").unwrap();
    table.reset_stats();
    // A literal that is not in the dictionary at all: the scan can prove no row matches without
    // comparing a single value.
    let r =
        ctx.sql("SELECT id FROM t WHERE kind = 'absent'").await.unwrap().collect().await.unwrap();
    assert_eq!(r.iter().map(|b| b.num_rows()).sum::<usize>(), 0);
    assert!(table.stats().batches_skipped >= 1, "a provably-empty predicate must skip, not scan");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn pushdown_never_changes_an_answer() {
    // The gate. Inexact pushdown is only allowed to return EXTRA rows; the engine re-filters. So for
    // every predicate shape, the SQL answer must be identical to the same query with the filter
    // applied by hand over a full scan.
    let dir = tmp("filtersame");
    let mut s = Store::open(&dir, cfg()).unwrap();
    let kinds = ["alpha", "beta", "gamma", "delta"];
    for p in 0..3 {
        for i in 0..200u32 {
            let v = p * 200 + i;
            s.put(
                &format!("q{v:04}"),
                &[Span::Lit(b"b")],
                vec![
                    ("kind".into(), AttrValue::Str(kinds[(v % 4) as usize].into())),
                    ("n".into(), AttrValue::Int(v as i64)),
                    ("ratio".into(), AttrValue::Float(v as f64 / 3.0)),
                    ("ok".into(), AttrValue::Bool(v % 7 == 0)),
                ],
            )
            .unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, _t) = TurndbTable::context(store, "t").unwrap();

    for pred in [
        "kind = 'beta'",
        "kind <> 'beta'",
        "kind < 'gamma'",
        "kind >= 'delta'",
        "kind = 'nonexistent'",
        "n > 400",
        "n <= 17",
        "500 < n",
        "ratio > 100.0",
        "ok = true",
        "kind = 'alpha' AND n > 300",
        "n > 100 AND n < 120 AND kind = 'alpha'",
    ] {
        let pushed = ctx
            .sql(&format!("SELECT count(*) AS c FROM t WHERE {pred}"))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let a =
            pushed[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0);

        // same predicate, but forced above a subquery the optimizer cannot push through
        let fenced = ctx
            .sql(&format!("SELECT count(*) AS c FROM (SELECT * FROM t ORDER BY id) WHERE {pred}"))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let b =
            fenced[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0);

        assert_eq!(a, b, "pushdown changed the answer for {pred:?}: {a} vs {b}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// Field-name collisions. Found by a panel; verified to brick the whole table at registration.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_attribute_named_like_a_builtin_does_not_brick_the_table() {
    // `id` and `body` are synthesised columns. An attribute of the same name is an ordinary thing for
    // a loader to produce, and it used to make DataFusion reject the table at REGISTRATION — so not
    // one bad column, but every query including `select count(*)`.
    let dir = tmp("collide");
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.put(
        "r1",
        &[Span::Lit(b"content")],
        vec![
            ("body".into(), AttrValue::Str("an attribute, not the body".into())),
            ("id".into(), AttrValue::Int(7)),
            ("fine".into(), AttrValue::Int(1)),
        ],
    )
    .unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, _t) = TurndbTable::context(store, "t").unwrap();
    let n = ctx.sql("SELECT count(*) AS n FROM t").await.unwrap().collect().await.unwrap();
    assert_eq!(
        n[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0),
        1
    );

    // the builtins keep their names and their meaning
    let r = ctx.sql("SELECT id, body FROM t").await.unwrap().collect().await.unwrap();
    assert_eq!(r[0].column(0).as_string::<i32>().value(0), "r1", "id is still the record id");
    assert_eq!(r[0].column(1).as_binary::<i32>().value(0), b"content", "body is still the content");

    // and the colliding attributes are still reachable, renamed rather than dropped
    let lens = Lens::new(&parts_of(&dir)).unwrap();
    let names: Vec<String> = lens.schema().fields().iter().map(|f| f.name().clone()).collect();
    assert!(
        names.contains(&"body#str".to_string()),
        "the colliding attribute is renamed: {names:?}"
    );
    assert!(names.contains(&"id#int".to_string()), "the colliding attribute is renamed: {names:?}");
    assert!(names.contains(&"fine".to_string()), "an uncontested key keeps its name");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_key_shaped_like_a_disambiguation_does_not_collide_either() {
    // A key literally named `a#str` beside a multi-typed `a` produces the same collision one level up.
    let dir = tmp("collide2");
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.put(
        "r1",
        &[Span::Lit(b"x")],
        vec![
            ("a".into(), AttrValue::Str("as string".into())),
            (
                "a#str".into(),
                AttrValue::Str("a literal key that looks like a disambiguation".into()),
            ),
        ],
    )
    .unwrap();
    s.put("r2", &[Span::Lit(b"y")], vec![("a".into(), AttrValue::Int(1))]).unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let store = Store::open_read(&dir, cfg()).unwrap();
    let (ctx, _t) = TurndbTable::context(store, "t").unwrap();
    let n = ctx.sql("SELECT count(*) AS n FROM t").await.unwrap().collect().await.unwrap();
    assert_eq!(
        n[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0),
        2
    );

    let lens = Lens::new(&parts_of(&dir)).unwrap();
    let names: Vec<String> = lens.schema().fields().iter().map(|f| f.name().clone()).collect();
    let uniq: std::collections::HashSet<_> = names.iter().collect();
    assert_eq!(uniq.len(), names.len(), "every field name must be unique: {names:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn zone_maps_prune_a_part_without_decoding_it() {
    use turndb::query::{Cmp, Pred};
    let dir = tmp("zoneprune");
    let mut s = Store::open(&dir, cfg()).unwrap();
    for i in 0..20i64 {
        s.put(&format!("a{i:02}"), &[Span::Lit(b"x")], vec![("tokens".into(), AttrValue::Int(i))])
            .unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    for i in 0..20i64 {
        s.put(
            &format!("b{i:02}"),
            &[Span::Lit(b"x")],
            vec![("tokens".into(), AttrValue::Int(10_000 + i))],
        )
        .unwrap();
    }
    s.sync().unwrap();
    s.flush().unwrap();
    drop(s);

    let parts = parts_of(&dir);
    assert_eq!(parts.len(), 2);
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["id"]).unwrap();
    let tokens_field =
        lens.schema().fields().iter().position(|f| f.name() == "tokens").expect("tokens in schema");
    let pred = Pred { field: tokens_field, op: Cmp::Gt, val: AttrValue::Int(5_000) };

    // Part 1 holds tokens [0, 19]: the zone DISPROVES the predicate, so the scan yields nothing
    // and decodes NO attribute section at all — the id projection needs none, and the predicate's
    // column was pruned before its rids were touched.
    let mut sc = lens.scan(&parts[0], None, &proj, std::slice::from_ref(&pred)).unwrap();
    assert!(sc.next_batch().unwrap().is_none(), "a zone-disproven part must yield nothing");
    let st = sc.stats();
    assert_eq!(st.rows, 0);
    assert_eq!(st.columns_decoded, 0, "pruning must decode no attribute section");

    // Part 2 holds tokens [10000, 10019]: the zone cannot disprove it, and every row survives.
    let mut sc = lens.scan(&parts[1], None, &proj, std::slice::from_ref(&pred)).unwrap();
    let mut rows = 0;
    while let Some(b) = sc.next_batch().unwrap() {
        rows += b.num_rows();
    }
    assert_eq!(rows, 20, "the matching part must be untouched by pruning");
    std::fs::remove_dir_all(&dir).ok();
}
