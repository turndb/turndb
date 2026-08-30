use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::schema::{AttrType, AttributeSchema, Schema};
use turndb::store::{ContentSpans, Store};
use turndb::types::AttrValue;

fn temp() -> PathBuf {
    // A per-process counter as well as the clock: Windows's clock can hand two parallel tests
    // the same nanosecond, and two tests in one directory then race at the store's create_new.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "turndb-schema-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn discovery_is_typed_namespaced_sorted_and_includes_the_live_memtable() {
    let dir = temp();
    let mut store = Store::open_file(&store_file(&dir), FoldCfg::default()).unwrap();
    store
        .put_record(
            "r1",
            &[
                ContentSpans::carve("response", b"large response", &turndb::carve::Carve::Whole),
                ContentSpans::carve("request", b"large request", &turndb::carve::Carve::Whole),
            ],
            vec![
                ("mixed".into(), AttrValue::Str("text".into())),
                ("z".into(), AttrValue::Bool(true)),
            ],
        )
        .unwrap();
    assert_eq!(
        store.schema().unwrap(),
        Schema {
            attributes: vec![
                AttributeSchema { name: "mixed".into(), types: vec![AttrType::String] },
                AttributeSchema { name: "z".into(), types: vec![AttrType::Bool] },
            ],
            contents: vec!["request".into(), "response".into()],
            may_include_shadowed_fields: false,
        }
    );

    store.sync().unwrap();
    store.flush().unwrap();
    store
        .put_record(
            "r2",
            &[ContentSpans::carve("aux", b"bytes", &turndb::carve::Carve::Whole)],
            vec![
                ("a".into(), AttrValue::Int(1)),
                ("mixed".into(), AttrValue::Float(-0.0)),
                ("mixed".into(), AttrValue::Int(i64::MIN)),
            ],
        )
        .unwrap();
    let before = store.fold().cache_stats();
    assert_eq!(
        store.schema().unwrap(),
        Schema {
            attributes: vec![
                AttributeSchema { name: "a".into(), types: vec![AttrType::Int] },
                AttributeSchema {
                    name: "mixed".into(),
                    types: vec![AttrType::String, AttrType::Int, AttrType::Float],
                },
                AttributeSchema { name: "z".into(), types: vec![AttrType::Bool] },
            ],
            contents: vec!["aux".into(), "request".into(), "response".into()],
            may_include_shadowed_fields: true,
        }
    );
    assert_eq!(store.fold().cache_stats().hits, before.hits);
    assert_eq!(store.fold().cache_stats().misses, before.misses);

    // Immutable discovery does not see the writer memtable and honestly marks part metadata as a
    // conservative physical superset.
    let reader = turndb::store::open_read_container(&store_file(&dir), FoldCfg::default()).unwrap();
    let schema = reader.schema().unwrap();
    assert_eq!(schema.contents, ["request", "response"]);
    assert!(schema.may_include_shadowed_fields);
    std::fs::remove_dir_all(dir).ok();
}

/// The migrated suites build single-file stores inside their temp directories: the parent is
/// ensured, the store is one file within it, and every cleanup keeps operating on the directory.
fn store_file(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).ok();
    dir.join("s.turndb")
}
