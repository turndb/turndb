//! The binding-independent Phase 3 semantic gate.
//!
//! This runner intentionally consumes JSON rather than constructing a second Rust-only fixture.
//! Node, Python, and browser runners can therefore replay the same writes and compare the same
//! logical views without inheriting Rust test helpers as an accidental specification.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use turndb::fold::FoldCfg;
use turndb::scan::{
    Compare, ContentMode, ContentSelect, Direction, Predicate, ProjectedContent, ScanInputError,
    ScanPage, ScanRequest, ScanRow,
};
use turndb::store::{open_read_container, Batch, ContentSpans, ReadStore, Span, Store};
use turndb::{AttrValue, ContentHash, Record};

const CORPUS_JSON: &str = include_str!("../conformance/v1/corpus.json");
const QUERY_SCHEMA_JSON: &str = include_str!("../conformance/v1/query.schema.json");
const CAPABILITIES_SCHEMA_JSON: &str = include_str!("../conformance/v1/capabilities.schema.json");
const CAPABILITIES_JSON: &str = include_str!("../conformance/v1/capabilities.json");
const CONTAINER_HEX: &str = include_str!("../conformance/v1/fixture.turndb.hex");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    contract_version: u8,
    steps: Vec<Step>,
    views: Vec<ViewFixture>,
    queries: Vec<QueryCase>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action")]
enum Step {
    #[serde(rename = "apply")]
    Apply { name: String, puts: Vec<RecordFixture>, deletes: Vec<String> },
    #[serde(rename = "sync")]
    Sync,
    #[serde(rename = "flush")]
    Flush,
    #[serde(rename = "captureSnapshot")]
    CaptureSnapshot { name: String },
    #[serde(rename = "assertWriter")]
    AssertWriter { name: String },
}

#[derive(Clone, Debug, Deserialize)]
struct ViewFixture {
    name: String,
    source: String,
    records: Vec<RecordFixture>,
}

#[derive(Clone, Debug, Deserialize)]
struct RecordFixture {
    id: String,
    attrs: Vec<AttrFixture>,
    contents: Vec<ContentFixture>,
}

#[derive(Clone, Debug, Deserialize)]
struct AttrFixture {
    name: String,
    value: ScalarFixture,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type")]
enum ScalarFixture {
    #[serde(rename = "string")]
    String { value: String },
    #[serde(rename = "i64")]
    I64 { decimal: String },
    #[serde(rename = "f64")]
    F64 {
        #[serde(rename = "bitsHex")]
        bits_hex: String,
    },
    #[serde(rename = "bool")]
    Bool { value: bool },
    #[serde(rename = "u64")]
    U64 { decimal: String },
    #[serde(rename = "binary")]
    Binary { base64: String },
    #[serde(rename = "timestampNs")]
    TimestampNs { decimal: String },
    #[serde(rename = "null")]
    Null,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum StorageFixture {
    Literal,
    Piece,
}

#[derive(Clone, Debug, Deserialize)]
struct ContentFixture {
    name: String,
    base64: String,
    storage: StorageFixture,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryCase {
    name: String,
    source: String,
    request: RequestFixture,
    #[serde(default)]
    paginate: bool,
    expected_ids: Vec<String>,
    #[serde(default)]
    assert_metadata_only_io: bool,
    #[serde(default)]
    assert_cursor_damage_rejected: bool,
    #[serde(default)]
    assert_cursor_mismatch_rejected: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestFixture {
    contract_version: u8,
    from: Option<String>,
    to: Option<String>,
    direction: Option<DirectionFixture>,
    cursor: Option<String>,
    limit: Option<usize>,
    max_examined: Option<usize>,
    max_resolution_entries: Option<usize>,
    max_reconstructed_bytes: Option<String>,
    #[serde(default)]
    attrs: Vec<String>,
    #[serde(default)]
    contents: Vec<ContentSelectFixture>,
    #[serde(default)]
    predicates: Vec<PredicateFixture>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum DirectionFixture {
    Forward,
    Reverse,
}

#[derive(Clone, Debug, Deserialize)]
struct ContentSelectFixture {
    name: String,
    mode: ContentModeFixture,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum ContentModeFixture {
    Metadata,
    Bytes,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind")]
enum PredicateFixture {
    #[serde(rename = "id")]
    Id { op: CompareFixture, value: String },
    #[serde(rename = "attr")]
    Attr { name: String, op: CompareFixture, value: ScalarFixture },
    #[serde(rename = "attrExists")]
    AttrExists { name: String, present: bool },
    #[serde(rename = "contentExists")]
    ContentExists { name: String, present: bool },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CompareFixture {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
}

trait ConformanceSource {
    fn get_record(&self, id: &str) -> anyhow::Result<Option<Record>>;
    fn reconstruct_named(&self, id: &str, name: &str) -> anyhow::Result<Option<Vec<u8>>>;
    fn scan_page(&self, request: &ScanRequest) -> anyhow::Result<ScanPage>;
}

impl ConformanceSource for Store {
    fn get_record(&self, id: &str) -> anyhow::Result<Option<Record>> {
        self.get(id)
    }

    fn reconstruct_named(&self, id: &str, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.reconstruct_content(id, name)
    }

    fn scan_page(&self, request: &ScanRequest) -> anyhow::Result<ScanPage> {
        self.scan(request)
    }
}

impl ConformanceSource for ReadStore {
    fn get_record(&self, id: &str) -> anyhow::Result<Option<Record>> {
        self.get(id)
    }

    fn reconstruct_named(&self, id: &str, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.reconstruct_content(id, name)
    }

    fn scan_page(&self, request: &ScanRequest) -> anyhow::Result<ScanPage> {
        self.scan(request)
    }
}

#[test]
fn contract_artifacts_are_complete_and_mutation_sensitive() {
    let query_schema: Value = serde_json::from_str(QUERY_SCHEMA_JSON).unwrap();
    let capabilities_schema: Value = serde_json::from_str(CAPABILITIES_SCHEMA_JSON).unwrap();
    let capabilities: Value = serde_json::from_str(CAPABILITIES_JSON).unwrap();
    let corpus: Corpus = serde_json::from_str(CORPUS_JSON).unwrap();

    assert_eq!(query_schema["$schema"], "https://json-schema.org/draft/2020-12/schema");
    assert_eq!(capabilities_schema["$schema"], "https://json-schema.org/draft/2020-12/schema");
    assert_eq!(query_schema["$defs"]["scalar"]["oneOf"].as_array().unwrap().len(), 8);
    assert_eq!(capabilities["contractVersion"], 1);
    assert_eq!(corpus.contract_version, 1);

    let operations = capabilities_schema["$defs"]["operation"]["enum"].as_array().unwrap();
    assert_eq!(operations.len(), operations.iter().collect::<HashSet<_>>().len());
    assert!(operations.iter().any(|operation| operation == "scan"));
    assert!(operations.iter().any(|operation| operation == "querySql"));

    validate_corpus(&corpus);

    // These deliberately mutated values must be refused. This stops the custom decoder from
    // quietly accepting representations that the JSON Schema and cross-language contract reject.
    assert!(parse_i64("01").is_err());
    assert!(parse_i64("+1").is_err());
    assert!(parse_i64("-0").is_err());
    assert!(parse_u64("-1").is_err());
    assert!(parse_u64("18446744073709551616").is_err());
    assert!(decode_f64_bits("7FF8000000000001").is_err());
    assert!(decode_f64_bits("000000000000000").is_err());
    assert!(decode_base64("AA=A").is_err());
    assert!(decode_base64("A").is_err());
}

#[test]
fn compiled_core_satisfies_capability_contract_invariants() {
    let fixture: Value = serde_json::from_str(CAPABILITIES_JSON).unwrap();
    let schema: Value = serde_json::from_str(CAPABILITIES_SCHEMA_JSON).unwrap();
    let core = turndb::capabilities::capabilities();
    let mut operations = vec![
        "openWriter",
        "openSnapshot",
        "compiledCapabilities",
        "write",
        "sync",
        "flush",
        "scan",
        "explainScan",
        "schema",
        "readContent",
        "seal",
        "verify",
        "spaceUsage",
        "compactBounded",
        "refold",
        "erase",
        "close",
    ];
    if core.sql {
        operations.push("querySql");
    }
    let native = json!({
        "contractVersion": 1,
        "profile": "native",
        "operations": operations,
        "partFormat": { "write": core.part_format_write, "readMax": core.part_format_read_max },
        "writerExclusion": if cfg!(unix) { "os_enforced" } else { "embedder_enforced" },
        "positionedIo": core.positioned_io,
        "threads": core.threads,
        "columnar": core.columnar,
        "sql": core.sql,
        "arrowIpc": core.columnar,
        "reclamation": if cfg!(unix) { "punch_or_refold" } else { "refold_only" },
        "cancellation": { "scan": true, "lifecycle": true }
    });
    validate_profile_shape(&native, &schema);
    validate_invariants(&native, &fixture);

    // The reduced profiles make platform loss explicit; the same invariant interpreter validates
    // them, so changing one rule exercises more than the native happy path.
    let browser = json!({
        "contractVersion": 1,
        "profile": "browser",
        "operations": ["openSnapshot", "compiledCapabilities", "scan", "explainScan", "schema", "readContent", "close"],
        "partFormat": { "readMax": core.part_format_read_max },
        "writerExclusion": "read_only",
        "positionedIo": true,
        "threads": false,
        "columnar": true,
        "sql": false,
        "arrowIpc": false,
        "reclamation": "none",
        "cancellation": { "scan": false, "lifecycle": false }
    });
    validate_profile_shape(&browser, &schema);
    validate_invariants(&browser, &fixture);

    let wasi = json!({
        "contractVersion": 1,
        "profile": "wasi",
        "operations": ["openWriter", "openSnapshot", "compiledCapabilities", "write", "sync", "flush", "scan", "explainScan", "schema", "readContent", "close"],
        "partFormat": { "write": core.part_format_write, "readMax": core.part_format_read_max },
        "writerExclusion": "embedder_enforced",
        "positionedIo": true,
        "threads": false,
        "columnar": false,
        "sql": false,
        "arrowIpc": false,
        "reclamation": "refold_only",
        "cancellation": { "scan": true, "lifecycle": true }
    });
    validate_profile_shape(&wasi, &schema);
    validate_invariants(&wasi, &fixture);
}

#[test]
fn rust_store_replays_the_shared_query_corpus() {
    let corpus: Corpus = serde_json::from_str(CORPUS_JSON).unwrap();
    validate_corpus(&corpus);

    let dir = temp_dir("query-v1");
    std::fs::create_dir_all(&dir).unwrap();
    let _remove = RemoveOnDrop(dir.clone());
    let path = dir.join("fixture.turndb");
    let mut writer = Store::open_file(&path, fold_cfg()).unwrap();
    let mut snapshots = HashMap::<String, ReadStore>::new();

    for step in &corpus.steps {
        match step {
            Step::Apply { name, puts, deletes } => {
                let mut batch = Batch::new();
                for record in puts {
                    stage_record(&mut batch, record).unwrap_or_else(|error| {
                        panic!("batch {name:?}, record {:?}: {error:#}", record.id)
                    });
                }
                for id in deletes {
                    batch.delete(id);
                }
                writer.apply(batch).unwrap_or_else(|error| panic!("apply {name:?}: {error:#}"));
            }
            Step::Sync => writer.sync().unwrap(),
            Step::Flush => {
                writer.flush().unwrap();
            }
            Step::CaptureSnapshot { name } => {
                let snapshot = open_read_container(&path, fold_cfg())
                    .unwrap_or_else(|error| panic!("capture snapshot {name:?}: {error:#}"));
                assert_source_cases(name, &snapshot, &corpus);
                assert!(snapshots.insert(name.clone(), snapshot).is_none());
            }
            Step::AssertWriter { name } => assert_source_cases(name, &writer, &corpus),
        }
    }

    // Re-run every immutable source after the whole timeline. In particular, snapshot-v1 must keep
    // beta and alpha's old values after the writer publishes v2.
    for (name, snapshot) in &snapshots {
        assert_source_cases(name, snapshot, &corpus);
    }

    let generated = std::fs::read(&path).unwrap();
    if std::env::var_os("TURNDB_UPDATE_CONFORMANCE_FIXTURE").is_some() {
        let mut encoded = String::new();
        for line in generated.chunks(64) {
            for byte in line {
                use std::fmt::Write as _;
                write!(&mut encoded, "{byte:02x}").unwrap();
            }
            encoded.push('\n');
        }
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/v1/fixture.turndb.hex"),
            encoded,
        )
        .unwrap();
        return;
    }
    assert_eq!(
        generated,
        decode_hex(CONTAINER_HEX).unwrap(),
        "the checked-in read-only fixture must be regenerated from corpus.json"
    );
}

#[test]
fn checked_in_container_matches_the_published_v2_view() {
    let corpus: Corpus = serde_json::from_str(CORPUS_JSON).unwrap();
    let dir = temp_dir("physical-fixture-v1");
    std::fs::create_dir_all(&dir).unwrap();
    let _remove = RemoveOnDrop(dir.clone());
    let path = dir.join("fixture.turndb");
    let bytes = decode_hex(CONTAINER_HEX).unwrap();
    assert_eq!(bytes.len(), 45_650, "fixture completeness guard");
    std::fs::write(&path, bytes).unwrap();
    let reader = open_read_container(&path, fold_cfg()).unwrap();
    assert_source_cases("snapshot-v2", &reader, &corpus);
}

#[derive(Clone)]
struct MemorySource(Arc<Vec<u8>>);

impl turndb::readat::ReadAt for MemorySource {
    fn read_exact_at(&self, into: &mut [u8], offset: u64) -> io::Result<()> {
        let offset = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        let bytes = self
            .0
            .get(offset..offset.saturating_add(into.len()))
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past memory source"))?;
        into.copy_from_slice(bytes);
        Ok(())
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.0.len() as u64)
    }
}

#[test]
fn arbitrary_positioned_source_matches_the_published_v2_view() {
    let corpus: Corpus = serde_json::from_str(CORPUS_JSON).unwrap();
    let bytes = Arc::new(decode_hex(CONTAINER_HEX).unwrap());
    let reader = turndb::store::open_read_container_source(
        Arc::new(MemorySource(bytes)),
        "memory://conformance-v1",
        fold_cfg(),
        turndb::read_limits::ReadLimits::default(),
    )
    .unwrap();
    assert_source_cases("snapshot-v2", &reader, &corpus);
}

fn validate_corpus(corpus: &Corpus) {
    assert_eq!(corpus.contract_version, 1);
    let mut sources = HashSet::new();
    let mut apply_names = HashSet::new();
    for step in &corpus.steps {
        match step {
            Step::Apply { name, puts, deletes } => {
                assert!(apply_names.insert(name), "duplicate apply name {name:?}");
                let mut ids = HashSet::new();
                for record in puts {
                    assert!(ids.insert(record.id.as_str()), "duplicate put id in {name:?}");
                    validate_record_fixture(record);
                }
                for id in deletes {
                    assert!(!id.is_empty());
                }
            }
            Step::CaptureSnapshot { name } | Step::AssertWriter { name } => {
                assert!(sources.insert(name), "duplicate source {name:?}");
            }
            Step::Sync | Step::Flush => {}
        }
    }

    let mut scalar_types = BTreeSet::new();
    let mut view_names = HashSet::new();
    for view in &corpus.views {
        assert!(view_names.insert(&view.name), "duplicate view name {:?}", view.name);
        assert!(sources.contains(&view.source), "unknown source {:?}", view.source);
        let mut previous: Option<&str> = None;
        for record in &view.records {
            validate_record_fixture(record);
            if let Some(previous) = previous {
                assert!(previous.as_bytes() < record.id.as_bytes(), "view ids must be byte-sorted");
            }
            previous = Some(&record.id);
            for attr in &record.attrs {
                scalar_types.insert(attr.value.kind());
            }
        }
    }
    assert_eq!(
        scalar_types,
        BTreeSet::from(["binary", "bool", "f64", "i64", "null", "string", "timestampNs", "u64"]),
        "the corpus must exercise every contract scalar"
    );

    let mut query_names = HashSet::new();
    for query in &corpus.queries {
        assert!(query_names.insert(&query.name), "duplicate query name {:?}", query.name);
        assert!(sources.contains(&query.source), "unknown source {:?}", query.source);
        assert_eq!(query.request.contract_version, 1);
        query.request.to_scan_request().unwrap();
    }
}

fn validate_record_fixture(record: &RecordFixture) {
    assert!(!record.id.is_empty());
    for attr in &record.attrs {
        assert!(!attr.name.is_empty());
        attr.value.to_attr_value().unwrap();
    }
    let mut names = HashSet::new();
    for content in &record.contents {
        assert!(!content.name.is_empty());
        assert!(names.insert(&content.name), "duplicate content name in {:?}", record.id);
        decode_base64(&content.base64).unwrap();
    }
}

fn stage_record(batch: &mut Batch, record: &RecordFixture) -> anyhow::Result<()> {
    let decoded: Vec<_> = record
        .contents
        .iter()
        .map(|content| Ok((content, decode_base64(&content.base64)?)))
        .collect::<anyhow::Result<_>>()?;
    let contents: Vec<_> = decoded
        .iter()
        .map(|(content, bytes)| {
            let span = match content.storage {
                StorageFixture::Literal => Span::Lit(bytes),
                StorageFixture::Piece => Span::Piece(bytes),
            };
            ContentSpans::new(&content.name, vec![span])
        })
        .collect();
    let attrs = record
        .attrs
        .iter()
        .map(|attr| Ok((attr.name.clone(), attr.value.to_attr_value()?)))
        .collect::<anyhow::Result<_>>()?;
    batch.put_record(&record.id, &contents, attrs)
}

fn assert_source_cases<S: ConformanceSource>(name: &str, source: &S, corpus: &Corpus) {
    let views: Vec<_> = corpus.views.iter().filter(|view| view.source == name).collect();
    assert_eq!(views.len(), 1, "source {name:?} must have exactly one golden view");
    assert_view(source, views[0]);
    for query in corpus.queries.iter().filter(|query| query.source == name) {
        assert_query(source, views[0], query);
    }
}

fn assert_view<S: ConformanceSource>(source: &S, view: &ViewFixture) {
    let page = source.scan_page(&ScanRequest { limit: 100, ..ScanRequest::default() }).unwrap();
    let actual_ids: Vec<_> = page.rows.iter().map(|row| row.id.as_str()).collect();
    let expected_ids: Vec<_> = view.records.iter().map(|record| record.id.as_str()).collect();
    assert_eq!(actual_ids, expected_ids, "logical ids for view {:?}", view.name);
    assert!(page.next.is_none());

    for expected in &view.records {
        let actual = source
            .get_record(&expected.id)
            .unwrap()
            .unwrap_or_else(|| panic!("view {:?} lost record {:?}", view.name, expected.id));
        let expected_attrs: Vec<_> = expected
            .attrs
            .iter()
            .map(|attr| (attr.name.clone(), attr.value.to_attr_value().unwrap()))
            .collect();
        assert_eq!(actual.attrs, expected_attrs, "attributes for {:?}", expected.id);
        assert_eq!(
            actual.contents.iter().map(|content| content.name.as_str()).collect::<Vec<_>>(),
            expected.contents.iter().map(|content| content.name.as_str()).collect::<Vec<_>>(),
            "content names for {:?}",
            expected.id
        );
        for expected_content in &expected.contents {
            let bytes = decode_base64(&expected_content.base64).unwrap();
            let actual_content = actual.content(&expected_content.name).unwrap();
            assert_eq!(actual_content.len(), bytes.len() as u64);
            assert_eq!(actual_content.identity, Some(ContentHash::of(&bytes)));
            assert_eq!(
                source.reconstruct_named(&expected.id, &expected_content.name).unwrap(),
                Some(bytes),
                "content {:?}/{:?}",
                expected.id,
                expected_content.name
            );
        }
    }
}

fn assert_query<S: ConformanceSource>(source: &S, view: &ViewFixture, case: &QueryCase) {
    let mut request = case.request.to_scan_request().unwrap();
    let first = source
        .scan_page(&request)
        .unwrap_or_else(|error| panic!("query {:?}: {error:#}", case.name));

    if case.assert_cursor_damage_rejected || case.assert_cursor_mismatch_rejected {
        let cursor = first.next.clone().expect("cursor-refusal case must produce a cursor");
        if case.assert_cursor_damage_rejected {
            let mut damaged = cursor.clone().into_bytes();
            let last = damaged.last_mut().unwrap();
            *last = if *last == b'A' { b'B' } else { b'A' };
            let mut bad_request = case.request.to_scan_request().unwrap();
            bad_request.cursor = Some(String::from_utf8(damaged).unwrap());
            let error = source.scan_page(&bad_request).unwrap_err();
            assert!(error.is::<ScanInputError>(), "damaged cursor error: {error:#}");
        }
        if case.assert_cursor_mismatch_rejected {
            let mut mismatch = case.request.to_scan_request().unwrap();
            mismatch.cursor = Some(cursor);
            mismatch
                .predicates
                .push(Predicate::Id { op: Compare::Ne, value: "__changed_after_cursor__".into() });
            let error = source.scan_page(&mismatch).unwrap_err();
            assert!(error.is::<ScanInputError>(), "mismatched cursor error: {error:#}");
        }
    }

    let mut pages = vec![first];
    if case.paginate {
        for _ in 0..100 {
            let Some(cursor) = pages.last().unwrap().next.clone() else {
                break;
            };
            request.cursor = Some(cursor);
            pages.push(
                source.scan_page(&request).unwrap_or_else(|error| {
                    panic!("query {:?} continuation: {error:#}", case.name)
                }),
            );
        }
        assert!(pages.last().unwrap().next.is_none(), "query {:?} did not terminate", case.name);
    } else {
        assert!(pages[0].next.is_none(), "unpaged query {:?} unexpectedly needs paging", case.name);
    }

    let rows: Vec<_> = pages.iter().flat_map(|page| page.rows.iter()).collect();
    let actual_ids: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
    let expected_ids: Vec<_> = case.expected_ids.iter().map(String::as_str).collect();
    assert_eq!(actual_ids, expected_ids, "query {:?}", case.name);

    for row in rows {
        let expected =
            view.records.iter().find(|record| record.id == row.id).unwrap_or_else(|| {
                panic!("query {:?} returned unknown id {:?}", case.name, row.id)
            });
        assert_projected_row(row, expected, &case.request);
    }
    for page in &pages {
        assert_eq!(page.stats.returned, page.rows.len());
        if case.assert_metadata_only_io {
            assert_eq!(page.stats.content_values_reconstructed, 0);
            assert_eq!(page.stats.reconstructed_bytes, 0);
            assert_eq!(page.stats.io.fold_blocks_touched, 0);
            assert_eq!(page.stats.io.fold_block_cache_hits, 0);
            assert_eq!(page.stats.io.fold_block_cache_misses, 0);
            assert_eq!(page.stats.io.fold_stored_bytes_read, 0);
            assert_eq!(page.stats.io.fold_raw_bytes_decoded, 0);
        }
    }
    if case.name == "all-scalars-duplicates-and-content-shapes" {
        assert_eq!(pages[0].stats.duplicate_attr_occurrences, 1);
    }
    if case.name == "content-budget-refuses-to-truncate" {
        assert!(pages.iter().all(|page| page.rows.len() == 1));
        assert!(pages[..pages.len() - 1]
            .iter()
            .all(|page| page.stats.reconstruction_budget_exhausted));
    }
}

fn assert_projected_row(row: &ScanRow, expected: &RecordFixture, request: &RequestFixture) {
    let selected: HashSet<_> = request.attrs.iter().map(String::as_str).collect();
    let expected_attrs: Vec<_> = expected
        .attrs
        .iter()
        .filter(|attr| selected.contains(attr.name.as_str()))
        .map(|attr| (attr.name.clone(), attr.value.to_attr_value().unwrap()))
        .collect();
    assert_eq!(row.attrs, expected_attrs, "projected attrs for {:?}", row.id);
    assert_eq!(row.contents.len(), request.contents.len());
    for (actual, selected) in row.contents.iter().zip(&request.contents) {
        let expected_content =
            expected.contents.iter().find(|content| content.name == selected.name);
        assert_projected_content(actual, selected, expected_content);
    }
}

fn assert_projected_content(
    actual: &ProjectedContent,
    selected: &ContentSelectFixture,
    expected: Option<&ContentFixture>,
) {
    assert_eq!(actual.name, selected.name);
    assert_eq!(actual.present, expected.is_some());
    let Some(expected) = expected else {
        assert_eq!(actual.len, None);
        assert_eq!(actual.pieces, None);
        assert_eq!(actual.identity, None);
        assert_eq!(actual.bytes, None);
        return;
    };
    let bytes = decode_base64(&expected.base64).unwrap();
    assert_eq!(actual.len, Some(bytes.len() as u64));
    assert_eq!(actual.pieces, Some(usize::from(expected.storage == StorageFixture::Piece)));
    assert_eq!(actual.identity, Some(ContentHash::of(&bytes)));
    match selected.mode {
        ContentModeFixture::Metadata => assert_eq!(actual.bytes, None),
        ContentModeFixture::Bytes => assert_eq!(actual.bytes.as_deref(), Some(bytes.as_slice())),
    }
}

impl RequestFixture {
    fn to_scan_request(&self) -> anyhow::Result<ScanRequest> {
        anyhow::ensure!(self.contract_version == 1, "unsupported query contract version");
        let mut request = ScanRequest::default();
        request.from.clone_from(&self.from);
        request.to.clone_from(&self.to);
        request.cursor.clone_from(&self.cursor);
        if let Some(direction) = self.direction {
            request.direction = match direction {
                DirectionFixture::Forward => Direction::Forward,
                DirectionFixture::Reverse => Direction::Reverse,
            };
        }
        if let Some(limit) = self.limit {
            request.limit = limit;
        }
        if let Some(max_examined) = self.max_examined {
            request.max_examined = max_examined;
        }
        if let Some(max_resolution_entries) = self.max_resolution_entries {
            request.max_resolution_entries = max_resolution_entries;
        }
        if let Some(value) = &self.max_reconstructed_bytes {
            request.max_reconstructed_bytes = parse_u64(value)?;
            anyhow::ensure!(request.max_reconstructed_bytes > 0, "zero reconstruction ceiling");
        }
        request.attrs.clone_from(&self.attrs);
        request.contents = self
            .contents
            .iter()
            .map(|selected| ContentSelect {
                name: selected.name.clone(),
                mode: match selected.mode {
                    ContentModeFixture::Metadata => ContentMode::Metadata,
                    ContentModeFixture::Bytes => ContentMode::Bytes,
                },
            })
            .collect();
        request.predicates = self
            .predicates
            .iter()
            .map(PredicateFixture::to_predicate)
            .collect::<anyhow::Result<_>>()?;
        Ok(request)
    }
}

impl PredicateFixture {
    fn to_predicate(&self) -> anyhow::Result<Predicate> {
        Ok(match self {
            PredicateFixture::Id { op, value } => {
                Predicate::Id { op: (*op).into(), value: value.clone() }
            }
            PredicateFixture::Attr { name, op, value } => Predicate::Attr {
                name: name.clone(),
                op: (*op).into(),
                value: value.to_attr_value()?,
            },
            PredicateFixture::AttrExists { name, present } => {
                Predicate::AttrExists { name: name.clone(), present: *present }
            }
            PredicateFixture::ContentExists { name, present } => {
                Predicate::ContentExists { name: name.clone(), present: *present }
            }
        })
    }
}

impl From<CompareFixture> for Compare {
    fn from(value: CompareFixture) -> Self {
        match value {
            CompareFixture::Eq => Compare::Eq,
            CompareFixture::Ne => Compare::Ne,
            CompareFixture::Lt => Compare::Lt,
            CompareFixture::Lte => Compare::LtEq,
            CompareFixture::Gt => Compare::Gt,
            CompareFixture::Gte => Compare::GtEq,
        }
    }
}

impl ScalarFixture {
    fn kind(&self) -> &'static str {
        match self {
            ScalarFixture::String { .. } => "string",
            ScalarFixture::I64 { .. } => "i64",
            ScalarFixture::F64 { .. } => "f64",
            ScalarFixture::Bool { .. } => "bool",
            ScalarFixture::U64 { .. } => "u64",
            ScalarFixture::Binary { .. } => "binary",
            ScalarFixture::TimestampNs { .. } => "timestampNs",
            ScalarFixture::Null => "null",
        }
    }

    fn to_attr_value(&self) -> anyhow::Result<AttrValue> {
        Ok(match self {
            ScalarFixture::String { value } => AttrValue::Str(value.clone()),
            ScalarFixture::I64 { decimal } => AttrValue::Int(parse_i64(decimal)?),
            ScalarFixture::F64 { bits_hex } => AttrValue::Float(decode_f64_bits(bits_hex)?),
            ScalarFixture::Bool { value } => AttrValue::Bool(*value),
            ScalarFixture::U64 { decimal } => AttrValue::UInt(parse_u64(decimal)?),
            ScalarFixture::Binary { base64 } => AttrValue::Bytes(decode_base64(base64)?),
            ScalarFixture::TimestampNs { decimal } => AttrValue::TimestampNs(parse_i64(decimal)?),
            ScalarFixture::Null => AttrValue::Null,
        })
    }
}

fn parse_i64(value: &str) -> anyhow::Result<i64> {
    anyhow::ensure!(canonical_decimal(value, true), "non-canonical i64 {value:?}");
    value.parse().map_err(Into::into)
}

fn parse_u64(value: &str) -> anyhow::Result<u64> {
    anyhow::ensure!(canonical_decimal(value, false), "non-canonical u64 {value:?}");
    value.parse().map_err(Into::into)
}

fn canonical_decimal(value: &str, signed: bool) -> bool {
    let digits = if signed { value.strip_prefix('-').unwrap_or(value) } else { value };
    if digits.is_empty() || (value.starts_with('-') && (!signed || digits == "0")) {
        return false;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return false;
    }
    digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn decode_f64_bits(value: &str) -> anyhow::Result<f64> {
    anyhow::ensure!(
        value.len() == 16
            && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid f64 bitsHex {value:?}"
    );
    Ok(f64::from_bits(u64::from_str_radix(value, 16)?))
}

fn decode_base64(value: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(value.len().is_multiple_of(4), "base64 length is not a multiple of four");
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (chunk_index, chunk) in bytes.chunks_exact(4).enumerate() {
        let last = chunk_index + 1 == bytes.len() / 4;
        let padding = usize::from(chunk[3] == b'=') + usize::from(chunk[2] == b'=');
        anyhow::ensure!(last || padding == 0, "base64 padding before final quartet");
        anyhow::ensure!(padding <= 2, "invalid base64 padding");
        anyhow::ensure!(chunk[0] != b'=' && chunk[1] != b'=', "invalid base64 padding");
        anyhow::ensure!(chunk[2] != b'=' || chunk[3] == b'=', "invalid base64 padding");
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c = if chunk[2] == b'=' { 0 } else { base64_value(chunk[2])? };
        let d = if chunk[3] == b'=' { 0 } else { base64_value(chunk[3])? };
        anyhow::ensure!(padding != 2 || b & 0x0f == 0, "non-canonical base64 tail bits");
        anyhow::ensure!(padding != 1 || c & 0x03 == 0, "non-canonical base64 tail bits");
        out.push((a << 2) | (b >> 4));
        if padding < 2 {
            out.push((b << 4) | (c >> 2));
        }
        if padding == 0 {
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

fn base64_value(byte: u8) -> anyhow::Result<u8> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => anyhow::bail!("invalid base64 character"),
    }
}

fn decode_hex(value: &str) -> anyhow::Result<Vec<u8>> {
    let digits: Vec<_> = value.bytes().filter(|byte| !byte.is_ascii_whitespace()).collect();
    anyhow::ensure!(digits.len().is_multiple_of(2), "hex fixture has a partial byte");
    digits
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair)?;
            Ok(u8::from_str_radix(text, 16)?)
        })
        .collect()
}

fn validate_profile_shape(profile: &Value, schema: &Value) {
    let object = profile.as_object().expect("profile object");
    for field in schema["required"].as_array().unwrap() {
        assert!(object.contains_key(field.as_str().unwrap()), "missing capability field {field}");
    }
    assert_eq!(profile["contractVersion"], 1);
    let allowed_profiles = schema["properties"]["profile"]["enum"].as_array().unwrap();
    assert!(allowed_profiles.contains(&profile["profile"]));
    let allowed_operations: HashSet<_> =
        schema["$defs"]["operation"]["enum"].as_array().unwrap().iter().collect();
    for operation in profile["operations"].as_array().unwrap() {
        assert!(allowed_operations.contains(operation), "unknown operation {operation}");
    }
    assert!(profile["partFormat"]["readMax"].as_u64().is_some());
    assert!(profile["positionedIo"].is_boolean());
    assert!(profile["threads"].is_boolean());
    assert!(profile["columnar"].is_boolean());
    assert!(profile["sql"].is_boolean());
    assert!(profile["arrowIpc"].is_boolean());
    assert!(profile["cancellation"]["scan"].is_boolean());
    assert!(profile["cancellation"]["lifecycle"].is_boolean());
}

fn validate_invariants(profile: &Value, fixture: &Value) {
    for invariant in fixture["invariants"].as_array().unwrap() {
        if condition_matches(profile, &invariant["if"]) {
            assert_then(profile, &invariant["then"], invariant["name"].as_str().unwrap());
        }
    }
}

fn condition_matches(profile: &Value, condition: &Value) -> bool {
    condition.as_object().unwrap().iter().all(|(key, expected)| match key.as_str() {
        "operationsInclude" => expected
            .as_array()
            .unwrap()
            .iter()
            .all(|operation| profile["operations"].as_array().unwrap().contains(operation)),
        _ => profile.get(key) == Some(expected),
    })
}

fn assert_then(profile: &Value, consequence: &Value, name: &str) {
    for (key, expected) in consequence.as_object().unwrap() {
        match key.as_str() {
            "operationsInclude" => {
                for operation in expected.as_array().unwrap() {
                    assert!(
                        profile["operations"].as_array().unwrap().contains(operation),
                        "capability invariant {name:?} requires operation {operation}"
                    );
                }
            }
            "operationsExclude" => {
                for operation in expected.as_array().unwrap() {
                    assert!(
                        !profile["operations"].as_array().unwrap().contains(operation),
                        "capability invariant {name:?} forbids operation {operation}"
                    );
                }
            }
            "partFormatWriteAbsent" => {
                assert_eq!(*expected, true);
                assert!(profile["partFormat"].get("write").is_none(), "invariant {name:?}");
            }
            "cancellationScan" => assert_eq!(profile["cancellation"]["scan"], *expected),
            _ => assert_eq!(profile[key], *expected, "capability invariant {name:?}, field {key}"),
        }
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-conformance-{tag}-{}-{now}", std::process::id()))
}

fn fold_cfg() -> FoldCfg {
    FoldCfg { seg_max: 1 << 22, ..FoldCfg::default() }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}
