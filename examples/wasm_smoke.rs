//! Round-trip proof for a non-native build: write a store, read it back byte-exact, seal it.
//!
//! Built for `wasm32-wasip1` and run under a WASI host, this exercises the whole engine through
//! the platform floor in `sys.rs` — positioned reads via preview1, the pure-Rust zstd encoder, the
//! degraded lock — and writes a store the NATIVE build is then asked to open. That second half is
//! the point: a WASM store must be an ordinary store, not a dialect.
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

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/store".into());
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(64);
    let path = std::path::Path::new(&dir);

    let mut s = Store::open(path, FoldCfg::default())?;
    for i in 0..n {
        s.put_body(
            &format!("member/{:013}/{i:04}#input", 1_700_000_000_000u64 + i as u64),
            &body(i),
            vec![
                ("model".into(), AttrValue::Str("claude-opus-5".into())),
                ("turn".into(), AttrValue::Int(i as i64)),
            ],
        )?;
    }
    s.sync()?;
    s.flush()?;

    // Byte-exact reconstruction — the cardinal invariant, checked in the build that wrote it.
    for i in 0..n {
        let id = format!("member/{:013}/{i:04}#input", 1_700_000_000_000u64 + i as u64);
        let got = s.reconstruct(&id)?.ok_or_else(|| anyhow::anyhow!("{id} missing"))?;
        anyhow::ensure!(got == body(i), "{id} did not round-trip byte-exact");
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
