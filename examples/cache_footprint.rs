//! What a Part pins in memory once it has been read. usage: cache_footprint <dir>

use std::path::PathBuf;
use turndb::part::cache::SectionCache;
use turndb::part::Part;

fn mib(b: usize) -> f64 { b as f64 / (1024.0 * 1024.0) }

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: cache_footprint <dir>"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "part").unwrap_or(false)).collect();
    paths.sort();

    // All parts share ONE budget, as a store's do.
    let shared = SectionCache::shared();
    let mut fs = 0u64;
    let mut open: Vec<Part> = Vec::new();
    for p in &paths {
        fs += std::fs::metadata(p)?.len();
        open.push(Part::open_in(p, shared.clone())?);
    }
    for part in &open {
        // a whole-part walk, exactly what merge does
        for r in 0..part.len() {
            let _ = part.record(r)?;
        }
    }
    println!("{} parts, {:.2} MiB on disk\n", paths.len(), fs as f64 / 1048576.0);
    println!("  pinned      {:>9.2} MiB   ({:.1}x on-disk)", mib(shared.bytes()),
        shared.bytes() as f64 / fs as f64);
    println!("  budget      {:>9.2} MiB", mib(shared.budget()));
    println!("  entries     {:>9}", shared.entries());
    println!("  per part    {:>9.2} MiB", mib(shared.bytes() / paths.len()));

    // and again against a budget a large store would actually hit
    drop(open);
    let tight = std::sync::Arc::new(SectionCache::new(32 << 20));
    let mut open2: Vec<Part> = Vec::new();
    for p in &paths {
        open2.push(Part::open_in(p, tight.clone())?);
    }
    for part in &open2 {
        for r in 0..part.len() {
            let _ = part.record(r)?;
        }
    }
    println!("\n  with a 32 MiB budget: pinned {:.2} MiB, {} entries",
        mib(tight.bytes()), tight.entries());
    Ok(())
}
