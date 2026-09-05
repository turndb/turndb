//! One store, every OS: the bytes TurnDB writes do not depend on the platform that wrote them.
//!
//! A reference store is built here from fixed inputs and compared BYTE FOR BYTE with a checked-in
//! current-writer copy made on Linux (`tests/fixtures/cross-os-reference.turndb.hex`). On Windows CI that is the
//! Linux→Windows direction; the Windows job also emits its own build as an artifact, and a Linux
//! job compares that back (`TURNDB_CROSS_OS_STORE`) — Windows→Linux. Each direction also opens,
//! verifies and reads the foreign bytes, so "opens" and "identical" are both asserted, and a
//! format byte that drifted on one platform fails here before anything else notices.

use std::path::{Path, PathBuf};
use turndb::fold::FoldCfg;
use turndb::store::{ContentSpans, Span, Store};
use turndb::AttrValue;

const FIXTURE: &str = "tests/fixtures/cross-os-reference.turndb.hex";

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("turndb-cross-os-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Fixed inputs: no clock, no randomness, no host names. Three commits so the manifest chain has
/// links; a tombstone and an erase so the fold and parts carry every record state; dedup across
/// records so the piece dictionary is exercised.
fn build_reference(path: &Path) {
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    let mut s = Store::open_file(path, cfg).unwrap();
    let shared = b"a shared prefix that deduplicates across records: ".to_vec();
    for round in 0..3u32 {
        for i in 0..8u32 {
            let id = format!("r{round}:{i:02}");
            let mut body = shared.clone();
            body.extend_from_slice(format!("round {round} record {i}").as_bytes());
            s.put_record(
                &id,
                &[
                    ContentSpans::new("body", vec![Span::Piece(&body)]),
                    ContentSpans::new(
                        "meta",
                        vec![
                            Span::Lit(b"{\"round\":"),
                            Span::Lit(round.to_string().as_bytes()),
                            Span::Lit(b"}"),
                        ],
                    ),
                ],
                vec![
                    ("round".into(), AttrValue::Int(i64::from(round))),
                    ("model".into(), AttrValue::Str(format!("m{}", i % 3))),
                    ("flag".into(), AttrValue::Bool(i % 2 == 0)),
                ],
            )
            .unwrap();
        }
        if round == 1 {
            s.delete("r0:00").unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let erased = s.erase_ids(&["r1:03".into()]).unwrap();
    assert_eq!(erased.tombstoned, 1);
    s.flush().unwrap();
    s.close().unwrap();
}

fn expected_ids() -> Vec<String> {
    let mut ids: Vec<String> = (0..3u32)
        .flat_map(|r| (0..8u32).map(move |i| format!("r{r}:{i:02}")))
        .filter(|id| id != "r0:00" && id != "r1:03")
        .collect();
    ids.sort();
    ids
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

fn read_hex_fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    let hex = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{name}: {e}"));
    let hex: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
}

fn read_fixture() -> Vec<u8> {
    read_hex_fixture(FIXTURE)
}

fn write_fixture(bytes: &[u8]) {
    let mut out = String::with_capacity(bytes.len() * 2 + bytes.len() / 32);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && i % 64 == 0 {
            out.push('\n');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out.push('\n');
    std::fs::write(fixture_path(), out).unwrap();
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    if a.len() != b.len() {
        return format!("lengths differ: {} vs {}", a.len(), b.len());
    }
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(at) => format!("first difference at byte {at}: {:02x} vs {:02x}", a[at], b[at]),
        None => "identical".into(),
    }
}

/// Open, verify every member and section, and reconstruct every expected record.
fn open_verify_read(path: &Path, what: &str) {
    let c = turndb::container::Container::open(path)
        .unwrap_or_else(|e| panic!("{what}: container refuses to open: {e:#}"));
    c.verify().unwrap_or_else(|e| panic!("{what}: verify failed: {e:#}"));
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    let rs = turndb::store::open_read_container(path, cfg)
        .unwrap_or_else(|e| panic!("{what}: read store refuses to open: {e:#}"));
    let mut ids = rs.ids().unwrap();
    ids.sort();
    assert_eq!(ids, expected_ids(), "{what}: ids");
    for id in &ids {
        let rec = rs.get(id).unwrap().unwrap_or_else(|| panic!("{what}: {id} absent"));
        assert_eq!(rec.contents.len(), 2, "{what}: {id} contents");
        let body = rs.reconstruct_content(id, "body").unwrap().unwrap();
        assert!(body.starts_with(b"a shared prefix"), "{what}: {id} body");
        assert_eq!(rec.attrs.len(), 3, "{what}: {id} attrs");
    }
}

#[test]
fn the_reference_build_is_deterministic_on_this_platform() {
    let d = tmp("det");
    build_reference(&d.join("a.turndb"));
    build_reference(&d.join("b.turndb"));
    let a = std::fs::read(d.join("a.turndb")).unwrap();
    let b = std::fs::read(d.join("b.turndb")).unwrap();
    assert!(a == b, "two builds from the same inputs differ: {}", first_difference(&a, &b));
    assert!(!d.join("a.turndb-wal").exists(), "a closed store leaves no sidecar");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_reference_build_is_byte_identical_to_the_linux_made_fixture() {
    let d = tmp("fixture");
    let here = d.join("here.turndb");
    build_reference(&here);
    let mine = std::fs::read(&here).unwrap();
    if std::env::var_os("TURNDB_WRITE_CROSS_OS_FIXTURE").is_some() {
        write_fixture(&mine);
        println!("wrote {} ({} bytes)", FIXTURE, mine.len());
    }
    if let Some(emit) = std::env::var_os("TURNDB_CROSS_OS_EMIT") {
        std::fs::copy(&here, &emit).unwrap();
        println!("emitted this platform's build to {}", Path::new(&emit).display());
    }
    let theirs = read_fixture();
    assert!(
        mine == theirs,
        "this platform's bytes differ from the Linux-made fixture: {}",
        first_difference(&mine, &theirs)
    );
    // The fixture's bytes, opened here — the foreign direction on every non-Linux platform.
    let foreign = d.join("fixture.turndb");
    std::fs::write(&foreign, &theirs).unwrap();
    open_verify_read(&foreign, "the Linux-made fixture opened here");
    let _ = std::fs::remove_dir_all(&d);
}

/// The other direction: a store built on another platform by this same test, handed over as a
/// CI artifact. Runs only where `TURNDB_CROSS_OS_STORE` points at one; the CI job that sets it
/// is required, so this cannot silently skip on the gate.
#[test]
fn a_store_made_on_another_platform_is_byte_identical_and_opens_here() {
    let Some(path) = std::env::var_os("TURNDB_CROSS_OS_STORE") else {
        println!("TURNDB_CROSS_OS_STORE not set; nothing foreign to compare on this run");
        return;
    };
    let path = PathBuf::from(path);
    let theirs = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let reference = read_fixture();
    assert!(
        theirs == reference,
        "{} differs from the reference fixture: {}",
        path.display(),
        first_difference(&theirs, &reference)
    );
    let d = tmp("foreign");
    let local = d.join("foreign.turndb");
    std::fs::write(&local, &theirs).unwrap();
    open_verify_read(&local, "the foreign-made store opened here");
    let _ = std::fs::remove_dir_all(&d);
}
