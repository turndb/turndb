//! Round-trip proof for a non-native build: write a store, read it back byte-exact, seal it.
//!
//! Built for `wasm32-wasip1` and run under a WASI host, this exercises the whole engine through the
//! platform floor in `sys.rs`. Built natively by `npm/interop.sh`, it supplies the reverse fixture
//! that the portable package must read. Either way, a store must remain ordinary format, not a
//! runtime dialect.
use anyhow::Result;
use turndb::fold::FoldCfg;
use turndb::store::Store;
use turndb::types::AttrValue;

fn body(i: usize) -> Vec<u8> {
    // Shaped like real traffic: a large shared prefix (the resent context) plus a unique tail, so
    // the fold's dedup and the compressor both have something real to do.
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
    let path = std::path::Path::new(&dir);

    let mut s = Store::open(path, FoldCfg::default())?;
    for i in 0..n {
        s.put_body(&id(i), &body(i), attrs(i))?;
    }
    s.sync()?;
    s.flush()?;

    // Byte-exact reconstruction — the cardinal invariant, checked in the build that wrote it.
    for i in 0..n {
        let id = id(i);
        let got = s.reconstruct(&id)?.ok_or_else(|| anyhow::anyhow!("{id} missing"))?;
        anyhow::ensure!(got == body(i), "{id} did not round-trip byte-exact");
        let record = s.get(&id)?.ok_or_else(|| anyhow::anyhow!("{id} metadata missing"))?;
        anyhow::ensure!(record.attrs == attrs(i), "{id} scalar metadata drifted");
    }

    // And the range read the integration depends on.
    let page = s.scan_ids(Some("member/"), None, 8, false)?;
    anyhow::ensure!(page.len() == 8, "expected a full page, got {}", page.len());

    let logical: usize = (0..n).map(|i| body(i).len()).sum();
    let on_disk = du(path)?;
    println!(
        "OK  {n} records  logical {:.2} MiB  on-disk {:.2} MiB  {:.1}x",
        logical as f64 / 1048576.0,
        on_disk as f64 / 1048576.0,
        logical as f64 / on_disk as f64
    );
    Ok(())
}

fn du(p: &std::path::Path) -> Result<u64> {
    let mut total = 0;
    for e in std::fs::read_dir(p)? {
        let e = e?;
        let m = e.metadata()?;
        total += if m.is_dir() { du(&e.path())? } else { m.len() };
    }
    Ok(total)
}
