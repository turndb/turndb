//! Time a merge over real parts. usage: merge_bench <dir> [n-parts]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use turndb::part::cache::SectionCache;
use turndb::part::{merge::merge, Part};

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: merge_bench <dir> [n]"));
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .collect();
    paths.sort();
    paths.truncate(n);
    // Optional third arg: section-cache budget in MiB, to price the floor the cache doc claims.
    let budget: usize = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(512);
    let cache = Arc::new(SectionCache::new(budget << 20));
    let parts: Vec<Arc<Part>> = paths
        .iter()
        .map(|p| Ok(Arc::new(Part::open_in(p, cache.clone())?)))
        .collect::<anyhow::Result<_>>()?;
    println!("section-cache budget {budget} MiB");
    let rows: usize = parts.iter().map(|p| p.len()).sum();
    println!("merging {} parts, {rows} records", parts.len());

    let out = dir.join("merged.tmp");
    let t = Instant::now();
    let (meta, st) = merge(&out, &parts, 19)?;
    let el = t.elapsed();
    let sz = std::fs::metadata(&out)?.len();
    println!(
        "  {:.1}s  ({:.0} rec/s)  {} in -> {} out, {} superseded, fold_bytes_touched={}  {:.2} MiB",
        el.as_secs_f64(),
        st.records_in as f64 / el.as_secs_f64(),
        st.records_in,
        meta.n_records,
        st.superseded,
        st.fold_bytes_touched,
        sz as f64 / (1024.0 * 1024.0)
    );
    std::fs::remove_file(&out).ok();
    Ok(())
}
