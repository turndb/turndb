//! The native-reader half of the interop proof: open a portable build's store and verify it exactly.
//!
//! Migration needs both directions. A backfill may run as a native binary while a Node process serves
//! reads, or the lightweight package may ingest before native maintenance takes over. The two builds
//! use different zstd implementations of the same format, so shared source types are not proof.
use anyhow::Result;
use turndb::fold::FoldCfg;
use turndb::types::AttrValue;

fn body(i: usize) -> Vec<u8> {
    let shared = "You are a careful assistant. Prior turn content repeated verbatim. ".repeat(200);
    format!("{shared}|unique turn {i}|{}", "x".repeat(i * 7)).into_bytes()
}

fn attrs(i: usize) -> Vec<(String, AttrValue)> {
    vec![
        ("model".into(), AttrValue::Str("cross-runtime".into())),
        ("turn".into(), AttrValue::Int(i as i64)),
        ("ratio".into(), AttrValue::Float(i as f64 / 7.0)),
        ("ok".into(), AttrValue::Bool(i.is_multiple_of(2))),
        ("tag".into(), AttrValue::Str("first".into())),
        ("u".into(), AttrValue::UInt(u64::MAX - i as u64)),
        ("raw".into(), AttrValue::Bytes(vec![0, i as u8, 255])),
        ("at".into(), AttrValue::TimestampNs(-1_700_000_000_000_000_000 + i as i64)),
        ("nothing".into(), AttrValue::Null),
        ("tag".into(), AttrValue::Str("second".into())),
    ]
}

fn id(i: usize) -> String {
    format!("member/{:013}/{i:04}#input", 1_700_000_000_000u64 + i as u64)
}

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/store".into());
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(64);
    let s = turndb::store::open_read_container(std::path::Path::new(&dir), FoldCfg::default())?;

    let ids = s.ids()?;
    let expected_ids: Vec<String> = (0..n).map(id).collect();
    anyhow::ensure!(ids == expected_ids, "id set/order differs across runtimes");
    let mut bytes = 0usize;
    for i in 0..n {
        let id = id(i);
        let got = s.reconstruct(&id)?.ok_or_else(|| anyhow::anyhow!("{id} missing"))?;
        anyhow::ensure!(got == body(i), "{id} content differs across runtimes");
        let record = s.get(&id)?.ok_or_else(|| anyhow::anyhow!("{id} metadata missing"))?;
        anyhow::ensure!(record.attrs == attrs(i), "{id} scalar metadata differs across runtimes");
        bytes += got.len();
    }
    // And the paged range read, which is what the binding will actually serve.
    let page = s.scan_ids(None, None, 5, false)?;
    println!(
        "OK  validated {} cross-runtime records / {} content bytes; first page {:?}",
        n,
        bytes,
        page.first()
    );
    Ok(())
}
