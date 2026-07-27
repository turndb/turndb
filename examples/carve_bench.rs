//! Compare carve strategies on a real JSONL corpus, through the real Store.
//!
//! usage: carve_bench <corpus.jsonl> <workdir> <cap> <spec>...
//!   spec: msg | msg:INTRA | cdc:TARGET | whole

use std::io::BufRead;
use std::path::PathBuf;
use turndb::carve::Carve;
use turndb::fold::FoldCfg;
use turndb::store::Store;

fn spec(s: &str) -> Carve {
    match s.split_once(':') {
        None if s == "msg" => Carve::Messages { intra: None },
        None if s == "whole" => Carve::Whole,
        Some(("msg", n)) => Carve::Messages { intra: Some(n.parse().unwrap()) },
        Some(("cdc", n)) => Carve::Cdc { target: n.parse().unwrap() },
        _ => panic!("unknown carve spec {s}"),
    }
}

fn dir_bytes(d: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            let p = e.path();
            total += if p.is_dir() { dir_bytes(&p) } else { e.metadata().map(|m| m.len()).unwrap_or(0) };
        }
    }
    total
}

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let corpus = PathBuf::from(a.next().expect("corpus.jsonl"));
    let work = PathBuf::from(a.next().expect("workdir"));
    let cap: usize = a.next().expect("cap").parse()?;
    let specs: Vec<String> = a.collect();

    let mut lines = Vec::with_capacity(cap);
    let rdr = std::io::BufReader::with_capacity(1 << 22, std::fs::File::open(&corpus)?);
    for line in rdr.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        lines.push(line);
        if lines.len() >= cap {
            break;
        }
    }
    println!("{} records", lines.len());

    for s in &specs {
        let carve = spec(s);
        let dir = work.join(s.replace(':', "-"));
        let _ = std::fs::remove_dir_all(&dir);
        let mut st = Store::open(&dir, FoldCfg::default())?;
        let t = std::time::Instant::now();
        let mut logical = 0u64;
        for (i, line) in lines.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(line)?;
            let Some(body) = v.get("body").and_then(|b| b.as_str()) else { continue };
            logical += body.len() as u64;
            st.put_body_with(&format!("r{i:07}"), body.as_bytes(), Vec::new(), &carve)?;
            if i % 4000 == 3999 {
                st.sync()?;
                st.flush()?;
            }
        }
        st.sync()?;
        st.flush()?;
        st.merge_range(0, st.part_count())?;
        let el = t.elapsed().as_secs_f64();
        let disk = dir_bytes(&dir);
        println!(
            "{s:<12} logical={:>7.1}MiB  disk={:>8.3}MiB  ratio={:>7.1}x  {el:>5.1}s",
            logical as f64 / (1 << 20) as f64,
            disk as f64 / (1 << 20) as f64,
            logical as f64 / disk as f64,
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(())
}
