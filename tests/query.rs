//! The query lens: columnar reads, projection, and SQL over a real store.
//!
//! The load-bearing test here is `columnar_and_row_paths_agree_exactly`. Two independent decoders now
//! read the same bytes — the row API walks the layout, the lens scatters columns — and a divergence
//! between them is a silent wrong answer, the worst failure this system can have.

#![cfg(feature = "sql")]

use datafusion::arrow::array::{Array, AsArray};
use std::path::PathBuf;
use std::sync::Arc;
use turndb::fold::FoldCfg;
use turndb::part::Part;
use turndb::query::{collect, table::TurndbTable, Lens};
use turndb::store::{Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-query-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    FoldCfg { seg_max: 1 << 22, ..FoldCfg::default() }
}

/// A store of `n` records per flush over `flushes` flushes, with a realistic attribute mix.
fn build(dir: &PathBuf, flushes: usize, per: usize) -> Vec<(String, Vec<u8>)> {
    let mut s = Store::open(dir, cfg()).unwrap();
    let mut want = Vec::new();
    for f in 0..flushes {
        for i in 0..per {
            let id = format!("t{f:02}-{i:04}");
            let body = format!("body of {id}, with enough text to be worth folding at all").into_bytes();
            let attrs = vec![
                ("model".to_string(), AttrValue::Str(if i % 3 == 0 { "opus" } else { "sonnet" }.into())),
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
    let mut ps: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().flatten()
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
    assert_eq!(names, vec!["id", "body", "only", "v#str", "v#int"],
        "a single-typed key keeps its name; a multi-typed key is split, never merged");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn parts_with_different_columns_share_one_row_shape() {
    let dir = tmp("union");
    let mut s = Store::open(&dir, cfg()).unwrap();
    s.put("a", &[Span::Lit(b"x")], vec![("early".into(), AttrValue::Int(1))]).unwrap();
    s.sync().unwrap(); s.flush().unwrap();
    // A column that did not exist when the first part was written.
    s.put("b", &[Span::Lit(b"y")], vec![("late".into(), AttrValue::Int(2))]).unwrap();
    s.sync().unwrap(); s.flush().unwrap();

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
    assert_eq!(seen, vec![
        ("a".to_string(), Some(1), None),
        ("b".to_string(), None, Some(2)),
    ], "a part lacking a column contributes nulls, not a schema mismatch");
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
    assert!(batches.iter().all(|b| b.num_columns() == 2), "only projected columns are materialised");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn projecting_body_without_a_fold_is_refused_rather_than_wrong() {
    let dir = tmp("nofold");
    build(&dir, 1, 10);
    let parts = parts_of(&dir);
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["id", "body"]).unwrap();
    assert!(collect(&parts, None, &lens, &proj).is_err(),
        "asking for content with no fold must fail, never return nulls");
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
            let s = match get("model") { AttrValue::Str(s) => s, v => panic!("model was {v:?}") };
            let t = match get("tokens") { AttrValue::Int(i) => i, v => panic!("tokens was {v:?}") };
            let c = match get("cost") { AttrValue::Float(f) => f, v => panic!("cost was {v:?}") };
            let o = match get("ok") { AttrValue::Bool(b) => b, v => panic!("ok was {v:?}") };
            expect.push((rec.id, s, t, c, o));
        }
    }
    assert_eq!(got.len(), expect.len());
    assert_eq!(got, expect, "THE COLUMNAR AND ROW DECODERS DISAGREE — one of them is silently wrong");

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
    s.put("r", &[Span::Lit(b"x")], vec![
        ("tag".into(), AttrValue::Str("first".into())),
        ("tag".into(), AttrValue::Str("second".into())),
        ("tag".into(), AttrValue::Str("third".into())),
    ]).unwrap();
    s.sync().unwrap(); s.flush().unwrap();

    let parts = parts_of(&dir);
    let lens = Lens::new(&parts).unwrap();
    let proj = lens.project(&["tag"]).unwrap();
    let (batches, stats) = collect(&parts, None, &lens, &proj).unwrap();
    let d = batches[0].column(0).as_dictionary::<datafusion::arrow::datatypes::Int32Type>();
    let v = d.values().as_string::<i32>();
    assert_eq!(v.value(d.key(0).unwrap()), "first", "the flat view takes the first occurrence");
    assert_eq!(stats.shadowed_occurrences, 2, "the other two must be COUNTED, not silently dropped");

    // and the substrate still holds all three, in order
    let rec = parts[0].record(0).unwrap();
    assert_eq!(rec.attrs.len(), 3, "the row path is lossless regardless of what the flat view shows");
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
    assert_eq!(n[0].column(0).as_primitive::<datafusion::arrow::datatypes::Int64Type>().value(0), 900);

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
    assert_eq!(rows, vec![("opus".to_string(), 300), ("sonnet".to_string(), 600)],
        "100 of every 300 rows are opus, across 3 parts");

    let f = ctx
        .sql("SELECT id FROM traces WHERE model = 'opus' AND tokens > 2900 ORDER BY id")
        .await.unwrap().collect().await.unwrap();
    let total: usize = f.iter().map(|b| b.num_rows()).sum();
    assert!(total > 0, "the filter must match something");
    for b in &f {
        let ids = b.column(0).as_string::<i32>();
        for i in 0..b.num_rows() {
            let n: usize = ids.value(i).split('-').nth(1).unwrap().parse().unwrap();
            assert!(n % 3 == 0 && n * 10 > 2900, "row {} does not satisfy the predicate", ids.value(i));
        }
    }
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
        .await.unwrap().collect().await.unwrap();

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

    let r = ctx.sql("SELECT id, body FROM traces WHERE id = 't00-0007'")
        .await.unwrap().collect().await.unwrap();
    let b = r.iter().find(|b| b.num_rows() > 0).expect("the row exists");
    let body = b.column(1).as_binary::<i32>().value(0);
    let expect = &want.iter().find(|(i, _)| i == "t00-0007").unwrap().1;
    assert_eq!(body, expect.as_slice(), "SQL must return content byte-exactly");
    assert!(table.stats().fold_reads > 0, "this query genuinely did read the fold");
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

    let mut sc = lens.scan(&parts[0], Some(&fold), &proj).unwrap();
    let (mut batches, mut rows, mut peak) = (0usize, 0usize, 0usize);
    let mut seen: Vec<Vec<u8>> = Vec::new();
    while let Some(b) = sc.next_batch().unwrap() {
        let bytes: usize = (0..b.num_rows()).map(|r| b.column(1).as_binary::<i32>().value(r).len()).sum();
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
    assert!(peak <= turndb::query::BATCH_BYTES + 300_000,
        "a batch carried {peak} content bytes, past the ceiling");
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
        s.put(&format!("s{i:04}"), &[Span::Piece(&body)], vec![("n".into(), AttrValue::Int(i as i64))]).unwrap();
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
    assert!(st.fold_reads < 300,
        "LIMIT 5 reconstructed {} of 300 bodies — the scan is not lazy", st.fold_reads);
    assert!(st.fold_reads >= 5, "it must have read at least the rows it returned");

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
                .flat_map(|j| blake3::hash(&(i * 77_777 + j).to_le_bytes()).as_bytes()[..32].to_vec())
                .collect();
            assert_eq!(bodies.value(r), want.as_slice(), "streamed body diverged for {}", ids.value(r));
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}
