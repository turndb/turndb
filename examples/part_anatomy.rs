//! Where do a part's bytes actually go?
//!
//! usage: part_anatomy <dir-with-parts>
//!
//! Parts turned out to be ~48% of a store's bytes on real data, against the ~15% assumed when arguing
//! that metadata-only compaction is nearly free. This prints the per-section breakdown so that claim
//! can be re-derived from evidence instead of restated.

use std::collections::BTreeMap;
use std::path::PathBuf;
use turndb::part::Part;

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

/// Collapse `col.rid.7` -> `col.rid.*` so per-column sections aggregate.
fn family(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((head, tail)) if tail.bytes().all(|b| b.is_ascii_digit()) => format!("{head}.*"),
        _ => name.to_string(),
    }
}

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: part_anatomy <dir>"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .collect();
    paths.sort();

    let mut agg: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new(); // stored, raw, count
    let (mut total_stored, mut records) = (0u64, 0u64);
    for p in &paths {
        let part = Part::open(p)?;
        records += part.len() as u64;
        for (name, stored, raw, _) in part.sections() {
            let e = agg.entry(family(&name)).or_insert((0, 0, 0));
            e.0 += stored as u64;
            e.1 += raw as u64;
            e.2 += 1;
            total_stored += stored as u64;
        }
    }
    let file_bytes: u64 =
        paths.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();

    println!("{} parts, {records} records, {:.2} MiB on disk\n", paths.len(), mib(file_bytes));
    println!(
        "{:<16}{:>11}{:>9}{:>12}{:>8}{:>12}",
        "section", "stored MiB", "% meta", "raw MiB", "ratio", "B/record"
    );
    let mut rows: Vec<_> = agg.into_iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1 .0));
    for (name, (stored, raw, _)) in &rows {
        println!(
            "{:<16}{:>11.3}{:>8.1}%{:>12.2}{:>7.1}x{:>12.1}",
            name,
            mib(*stored),
            *stored as f64 * 100.0 / total_stored as f64,
            mib(*raw),
            *raw as f64 / (*stored).max(1) as f64,
            *stored as f64 / records as f64
        );
    }
    println!("\n{:<16}{:>11.3}{:>8.1}%", "TOTAL", mib(total_stored), 100.0);
    println!("{:<16}{:>11.3}   (footers + TOCs)", "file overhead", mib(file_bytes - total_stored));
    Ok(())
}
