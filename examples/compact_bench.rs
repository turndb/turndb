//! Compaction policy, measured before it is chosen: replay a real corpus through the Store under
//! several policies and report what each actually costs and buys.
//!
//! usage: compact_bench <corpus.jsonl> <workdir> <cap> <policy>...
//!   policy: none | tiered:TRIGGER,RUN | total:K
//!
//! Reported per policy: ingest wall (Tier-1 dedup is O(parts), so policy shows up HERE, not just
//! in reads), merges run and their total wall, metadata bytes written by merges (the write amp
//! that remains when content never rewrites), final part count, and point-lookup latency over a
//! sample of ids.

use std::io::BufRead;
use std::path::PathBuf;
use std::time::Instant;
use turndb::fold::FoldCfg;
use turndb::store::Store;

enum Policy {
    None,
    Tiered { trigger: usize, run: usize },
    Total { k: usize },
}

fn parse(s: &str) -> Policy {
    match s.split_once(':') {
        None if s == "none" => Policy::None,
        Some(("tiered", args)) => {
            let (t, r) = args.split_once(',').expect("tiered:TRIGGER,RUN");
            Policy::Tiered { trigger: t.parse().unwrap(), run: r.parse().unwrap() }
        }
        Some(("total", k)) => Policy::Total { k: k.parse().unwrap() },
        _ => panic!("unknown policy {s}"),
    }
}

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let corpus = PathBuf::from(a.next().expect("corpus.jsonl"));
    let work = PathBuf::from(a.next().expect("workdir"));
    let cap: usize = a.next().expect("cap").parse()?;
    let specs: Vec<String> = a.collect();

    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(cap);
    let rdr = std::io::BufReader::with_capacity(1 << 22, std::fs::File::open(&corpus)?);
    for line in rdr.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&line)?;
        if let Some(b) = v.get("body").and_then(|b| b.as_str()) {
            bodies.push(b.as_bytes().to_vec());
            if bodies.len() >= cap {
                break;
            }
        }
    }
    println!("{} records, flush every 1000", bodies.len());

    for s in &specs {
        let policy = parse(s);
        let dir = work.join(s.replace([':', ','], "-"));
        let _ = std::fs::remove_dir_all(&dir);
        let mut st = Store::open(&dir, FoldCfg::default())?;
        let t0 = Instant::now();
        let (mut merges, mut merge_wall, mut merge_meta_bytes) = (0usize, 0.0f64, 0u64);
        for (i, body) in bodies.iter().enumerate() {
            st.put_body(&format!("r{i:07}"), body, Vec::new())?;
            if i % 1000 == 999 {
                st.sync()?;
                st.flush()?;
                let before: u64 = dir_part_bytes(&dir);
                let tm = Instant::now();
                let merged = match policy {
                    Policy::None => None,
                    Policy::Tiered { trigger, run } => st.maybe_compact(trigger, run)?,
                    Policy::Total { k } => {
                        if st.part_count() >= k {
                            st.merge_range(0, st.part_count())?
                        } else {
                            None
                        }
                    }
                };
                if merged.is_some() {
                    merges += 1;
                    merge_wall += tm.elapsed().as_secs_f64();
                    merge_meta_bytes += dir_part_bytes(&dir).saturating_sub(before / 2); // new part written
                }
            }
        }
        st.sync()?;
        st.flush()?;
        let ingest = t0.elapsed().as_secs_f64();

        // point lookups over a spread of ids
        let tl = Instant::now();
        let n_lookup = 2000.min(bodies.len());
        for j in 0..n_lookup {
            let id = format!("r{:07}", j * (bodies.len() / n_lookup));
            let _ = st.get(&id)?;
        }
        let lookup_ms = tl.elapsed().as_secs_f64() * 1000.0 / n_lookup as f64;

        println!(
            "{s:<14} ingest={ingest:>6.1}s  merges={merges:>3} ({merge_wall:>5.1}s, {:>6.1}MiB meta)  parts={:>3}  lookup={lookup_ms:.3}ms",
            merge_meta_bytes as f64 / (1 << 20) as f64,
            st.part_count(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(())
}

fn dir_part_bytes(d: &std::path::Path) -> u64 {
    std::fs::read_dir(d)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}
