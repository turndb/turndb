//! Time a read-only fold open — the sidecar-vs-scan measurement.
//!
//! usage: open_bench <fold-dir> [iters]

use std::path::PathBuf;
use std::time::Instant;
use turndb::fold::{Fold, FoldCfg};

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: open_bench <fold-dir> [iters]"));
    let iters: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let mut best = f64::MAX;
    for _ in 0..iters {
        let t = Instant::now();
        let f = Fold::open_read(&dir, FoldCfg::default())?;
        let el = t.elapsed().as_secs_f64() * 1000.0;
        best = best.min(el);
        drop(f);
    }
    println!("open_read best of {iters}: {best:.1} ms");
    Ok(())
}
