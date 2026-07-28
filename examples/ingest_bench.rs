//! Ingest throughput, measured identically on native and WASM.
//!
//! The number that matters for the binding: compression was ~80% of native ingest wall time and
//! runs INLINE on wasm32 (no threads), so the gap here is the honest cost of the target and is
//! what a flush cadence should be set from.
//!
//! Same corpus shape both sides — a large shared prefix (the resent context) plus a unique tail,
//! which is what real agent traffic looks like and what makes the fold's dedup meaningful.
use anyhow::Result;
use turndb::fold::FoldCfg;
use turndb::store::Store;
use turndb::types::AttrValue;

fn body(i: usize) -> Vec<u8> {
    let shared = "You are a careful assistant. Prior turn content repeated verbatim. ".repeat(200);
    format!("{shared}|unique turn {i}|{}", "x".repeat((i % 97) * 137)).into_bytes()
}

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/store".into());
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let flush_every: usize = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(512);
    let path = std::path::Path::new(&dir);

    let bodies: Vec<Vec<u8>> = (0..n).map(body).collect();
    let logical: usize = bodies.iter().map(|b| b.len()).sum();

    let t0 = std::time::Instant::now();
    let mut s = Store::open(path, FoldCfg::default())?;
    for (i, b) in bodies.iter().enumerate() {
        s.put_body(
            &format!("m/{:013}/{i:06}#input", 1_700_000_000_000u64 + i as u64),
            b,
            vec![("model".into(), AttrValue::Str("claude-opus-5".into()))],
        )?;
        if (i + 1) % flush_every == 0 {
            s.sync()?;
            s.flush()?;
        }
    }
    s.sync()?;
    s.flush()?;
    let secs = t0.elapsed().as_secs_f64();

    let on_disk = du(path)?;
    println!(
        "{n} records | {:.2} MiB logical | {:.3} MiB disk ({:.1}x) | {:.2}s | {:.0} rec/s | {:.1} MiB/s",
        logical as f64 / 1048576.0,
        on_disk as f64 / 1048576.0,
        logical as f64 / on_disk as f64,
        secs,
        n as f64 / secs,
        (logical as f64 / 1048576.0) / secs
    );
    Ok(())
}

fn du(p: &std::path::Path) -> Result<u64> {
    let mut t = 0;
    for e in std::fs::read_dir(p)? {
        let e = e?;
        let m = e.metadata()?;
        t += if m.is_dir() { du(&e.path())? } else { m.len() };
    }
    Ok(t)
}
