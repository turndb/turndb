//! Merge every part in a directory into one — the wall-clock and peak-RSS measurement, run under
//! /usr/bin/time against both the materialized and streaming builds.
//!
//! usage: merge_driver <corpus-dir> <out.part>

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use turndb::part::{merge, Part};

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: merge_driver <dir> <out>"));
    let out = PathBuf::from(std::env::args().nth(2).expect("usage: merge_driver <dir> <out>"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .collect();
    paths.sort();
    let parts: Vec<Arc<Part>> =
        paths.iter().map(|p| Ok(Arc::new(Part::open(p)?))).collect::<anyhow::Result<_>>()?;
    let records: usize = parts.iter().map(|p| p.len()).sum();
    let t = Instant::now();
    let (meta, stats) = merge::merge(&out, &parts, 3)?;
    println!(
        "merged {} parts / {records} records -> {} records in {:.2}s (fold bytes touched: {})",
        stats.inputs,
        meta.n_records,
        t.elapsed().as_secs_f64(),
        stats.fold_bytes_touched
    );
    Ok(())
}
