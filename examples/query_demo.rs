//! What a projected query actually touches, on a real store.
//!
//! usage: query_demo <store-dir-with-parts>

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use turndb::fold::{Fold, FoldCfg};
use turndb::part::Part;
use turndb::query::{Lens, ScanStats};

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: query_demo <dir>"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .collect();
    paths.sort();
    let parts: Vec<Arc<Part>> = paths.iter().map(|p| Ok(Arc::new(Part::open(p)?))).collect::<anyhow::Result<_>>()?;
    let fold = Fold::open_read(&dir.join("fold"), FoldCfg::default())?;
    let lens = Lens::new(&parts)?;

    let names: Vec<String> = lens.schema().fields().iter().map(|f| f.name().clone()).collect();
    println!("{} parts, schema has {} columns\n  {}\n", parts.len(), names.len(), names.join(", "));

    let rows: usize = parts.iter().map(|p| p.len()).sum();
    // STREAMED, never collected: a projected `body` column over this corpus is ~19 GiB of content,
    // so batches are measured and dropped. Peak residency is one batch, whatever the projection.
    for (label, cols, need_fold) in [
        ("one attribute", vec!["gen_ai.request.model"], false),
        ("four attributes", vec!["gen_ai.request.model", "gen_ai.response.model",
                                 "turndb.source.repo", "turndb.call_index"], false),
        ("ids only", vec!["id"], false),
        ("ids + BODY", vec!["id", "body"], true),
    ] {
        let Ok(proj) = lens.project(&cols) else { println!("{label:<18} (columns absent)"); continue };
        let t = Instant::now();
        let mut st = ScanStats::default();
        let mut bytes = 0usize;
        let mut peak = 0usize;
        for p in &parts {
            let mut sc = lens.scan(p, need_fold.then_some(&fold), &proj, &mut st)?;
            while let Some(b) = sc.next_batch()? {
                let n = b.get_array_memory_size();
                bytes += n;
                peak = peak.max(n);
            }
        }
        let el = t.elapsed();
        println!(
            "{label:<18} {:>7.2}s  {:>9} rows  fold_reads={:<8} arrow={:>8.1} MiB  peak batch={:.1} MiB",
            el.as_secs_f64(), st.rows, st.fold_reads,
            bytes as f64 / (1024.0 * 1024.0), peak as f64 / (1024.0 * 1024.0)
        );
    }
    println!("\n{rows} rows total");
    Ok(())
}
