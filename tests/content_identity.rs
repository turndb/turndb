use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::scan::{ContentMode, ContentSelect, ScanRequest};
use turndb::store::{ContentSpans, Span, Store};
use turndb::types::ContentHash;

fn temp() -> PathBuf {
    // A per-process counter as well as the clock: Windows's clock can hand two parallel tests
    // the same nanosecond, and two tests in one directory then race at the store's create_new.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "turndb-content-identity-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn metadata_request() -> ScanRequest {
    ScanRequest {
        contents: vec![ContentSelect { name: "payload".into(), mode: ContentMode::Metadata }],
        ..ScanRequest::default()
    }
}

#[test]
fn whole_value_identity_is_exact_carving_independent_and_metadata_only() {
    let dir = temp();
    let bytes = b"the exact same logical bytes across different carving boundaries";
    let split = 19;
    let mut store = Store::open_file(&store_file(&dir), FoldCfg::default()).unwrap();

    store
        .put_record("a", &[ContentSpans::new("payload", vec![Span::Piece(bytes)])], vec![])
        .unwrap();
    store
        .put_record(
            "b",
            &[ContentSpans::new(
                "payload",
                vec![Span::Piece(&bytes[..split]), Span::Piece(&bytes[split..])],
            )],
            vec![],
        )
        .unwrap();

    let expected = ContentHash::of(bytes);
    let before = store.fold().cache_stats();
    let page = store.scan(&metadata_request()).unwrap();
    assert_eq!(page.rows.len(), 2);
    assert!(page.rows.iter().all(|row| row.contents[0].identity == Some(expected)));
    assert_ne!(page.rows[0].contents[0].pieces, page.rows[1].contents[0].pieces);
    assert!(page.rows.iter().all(|row| row.contents[0].bytes.is_none()));
    assert_eq!(store.fold().cache_stats().hits, before.hits);
    assert_eq!(store.fold().cache_stats().misses, before.misses);

    store.sync().unwrap();
    store.flush().unwrap();
    let reader = turndb::store::open_read_container(&store_file(&dir), FoldCfg::default()).unwrap();
    let before = reader.fold().cache_stats();
    let page = reader.scan(&metadata_request()).unwrap();
    assert!(page.rows.iter().all(|row| row.contents[0].identity == Some(expected)));
    assert_eq!(reader.fold().cache_stats().hits, before.hits);
    assert_eq!(reader.fold().cache_stats().misses, before.misses);

    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn identities_survive_wal_replay_and_streaming_merge() {
    let dir = temp();
    let mut store = Store::open_file(&store_file(&dir), FoldCfg::default()).unwrap();
    store.put_body("a", b"first exact value", vec![]).unwrap();
    store.sync().unwrap();
    drop(store);

    let mut store = Store::open_file(&store_file(&dir), FoldCfg::default()).unwrap();
    assert_eq!(
        store
            .scan(&ScanRequest {
                contents: vec![ContentSelect { name: "body".into(), mode: ContentMode::Metadata }],
                ..ScanRequest::default()
            })
            .unwrap()
            .rows[0]
            .contents[0]
            .identity,
        Some(ContentHash::of(b"first exact value"))
    );
    store.flush().unwrap();
    store.put_body("b", b"second exact value", vec![]).unwrap();
    store.sync().unwrap();
    store.flush().unwrap();
    store.merge_range(0, 2).unwrap().unwrap();

    let reader = turndb::store::open_read_container(&store_file(&dir), FoldCfg::default()).unwrap();
    let page = reader
        .scan(&ScanRequest {
            contents: vec![ContentSelect { name: "body".into(), mode: ContentMode::Metadata }],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(page.rows[0].contents[0].identity, Some(ContentHash::of(b"first exact value")));
    assert_eq!(page.rows[1].contents[0].identity, Some(ContentHash::of(b"second exact value")));

    std::fs::remove_dir_all(dir).ok();
}

/// Build the suite's single-file store inside its cleanup directory.
fn store_file(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).ok();
    dir.join("s.turndb")
}
