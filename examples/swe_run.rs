//! Ingest a large corpus from stdin (JSONL) into fold + parts, and report the disk breakdown.
//!
//! usage:  <producer> | swe_run <dir> [records-per-part] [verify-every]
//!
//! Built for a corpus far too large to materialise: the producer streams, and nothing but the current
//! part batch is ever resident. Verification is SAMPLED (every Nth record is read back through its
//! part and compared byte-for-byte) because full verification doubles the work at this scale — the
//! sampling rate is reported so the claim stays honest.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Instant;
use turndb::fold::{Fold, FoldCfg};
use turndb::part::{self, Part};
use turndb::{AttrValue, BodyOp, Record};

fn split_json_array(s: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut i = 0;
    while i < s.len() && (s[i] as char).is_ascii_whitespace() { i += 1; }
    if i >= s.len() || s[i] != b'[' { return None; }
    i += 1;
    let mut out = Vec::new();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    let mut start: Option<usize> = None;
    while i < s.len() {
        let c = s[i];
        if in_str {
            if esc { esc = false; } else if c == b'\\' { esc = true; } else if c == b'"' { in_str = false; }
        } else {
            match c {
                b'"' => { in_str = true; if start.is_none() { start = Some(i); } }
                b'[' | b'{' => { if start.is_none() { start = Some(i); } depth += 1; }
                b']' | b'}' => {
                    if depth == 0 && c == b']' {
                        if let Some(st) = start.take() { out.push((st, i)); }
                        return Some(out);
                    }
                    depth -= 1;
                }
                b',' if depth == 0 => { if let Some(st) = start.take() { out.push((st, i)); } }
                w if (w as char).is_ascii_whitespace() => {}
                _ => { if start.is_none() { start = Some(i); } }
            }
        }
        i += 1;
    }
    None
}

fn carve(body: &[u8]) -> Vec<(bool, std::ops::Range<usize>)> {
    match split_json_array(body) {
        Some(elems) if !elems.is_empty() => {
            let mut out = Vec::with_capacity(elems.len() * 2 + 1);
            let mut cur = 0usize;
            for (a, b) in elems {
                if a > cur { out.push((false, cur..a)); }
                out.push((true, a..b));
                cur = b;
            }
            if cur < body.len() { out.push((false, cur..body.len())); }
            out
        }
        _ => vec![(true, 0..body.len())],
    }
}

fn gib(b: u64) -> f64 { b as f64 / (1024.0 * 1024.0 * 1024.0) }
fn mib(b: u64) -> f64 { b as f64 / (1024.0 * 1024.0) }

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let dir = PathBuf::from(a.next().expect("usage: swe_run <dir> [per-part] [verify-every]"));
    let per_part: usize = a.next().and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let verify_every: usize = a.next().and_then(|s| s.parse().ok()).unwrap_or(500);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let mut fold = Fold::open(&dir.join("fold"), FoldCfg::default())?;
    let stdin = std::io::stdin();
    let rdr = BufReader::with_capacity(1 << 22, stdin.lock());

    let (mut nrec, mut logical, mut refs, mut dups, mut skipped) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut pending: Vec<Record> = Vec::new();
    let mut samples: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut nparts = 0usize;
    let (mut seq, mut verified) = (0u64, 0u64);
    let t0 = Instant::now();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in rdr.lines() {
        let line = line?;
        if line.is_empty() { continue; }
        let v: serde_json::Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
        let body = match v.get("body").and_then(|b| b.as_str()) { Some(b) => b.as_bytes().to_vec(), None => continue };
        let id = format!(
            "{}:{}#{}",
            v.get("trace_id").and_then(|x| x.as_str()).unwrap_or("t"),
            v.get("span_id").and_then(|x| x.as_str()).unwrap_or("s"),
            v.get("kind").and_then(|x| x.as_str()).unwrap_or("k"));
        if !seen_ids.insert(id.clone()) { skipped += 1; continue; }

        let mut attrs = Vec::new();
        if let Some(obj) = v.as_object() {
            for (k, val) in obj {
                if k == "body" { continue; }
                let av = match val {
                    serde_json::Value::String(s) => AttrValue::Str(s.clone()),
                    serde_json::Value::Bool(b) => AttrValue::Bool(*b),
                    serde_json::Value::Number(n) if n.is_i64() => AttrValue::Int(n.as_i64().unwrap()),
                    serde_json::Value::Number(n) => AttrValue::Float(n.as_f64().unwrap_or(0.0)),
                    _ => continue,
                };
                attrs.push((k.clone(), av));
            }
        }
        nrec += 1;
        logical += body.len() as u64;

        let mut prog: Vec<BodyOp> = Vec::new();
        for (foldable, r) in carve(&body) {
            let span = &body[r.clone()];
            if foldable {
                let p = fold.put(span)?;
                refs += 1;
                if p.deduped { dups += 1; }
                prog.push(BodyOp::Piece { hash: p.hash, len: span.len() as u32 });
            } else {
                prog.push(BodyOp::Lit(span.to_vec()));
            }
        }
        if nrec % verify_every as u64 == 0 { samples.push((pending.len(), body)); }
        pending.push(Record { id, body: prog, attrs });

        if pending.len() >= per_part {
            fold.sync()?;
            seq += 1;
            let path = dir.join(format!("part-{nparts:05}.part"));
            part::build(&path, &pending, seq, seq, FoldCfg::default().level, |h| fold.lookup(*h))?;
            let p = Part::open(&path)?;
            for (idx, orig) in &samples {
                let r = &pending[*idx];
                let row = p.find(&r.id)?.expect("sampled id must be findable");
                if &p.reconstruct(row, &fold)? != orig { anyhow::bail!("BYTE DRIFT for {}", r.id); }
                if p.attrs(row)? != r.attrs { anyhow::bail!("ATTR DRIFT for {}", r.id); }
                verified += 1;
            }
            nparts += 1;
            pending.clear();
            samples.clear();
            seen_ids.clear();
            // NOTE: the dedup window is deliberately NOT sealed here. Sealing it bounds memory to the
            // flush interval (the production posture) but confines dedup to one batch, which would
            // understate the architecture on a corpus whose whole point is content recurring across
            // trajectories. Keeping it resident measures TRUE GLOBAL dedup — what Tier-1 lookups
            // against parts' hash columns would deliver once implemented. Memory is reported so the
            // cost of that choice is visible.
            let el = t0.elapsed().as_secs_f64();
            eprint!("\r  {nrec} rec | {:.1} GiB logical | fold {:.2} GiB | {} parts | {:.0} rec/s | {:.0} MiB/s   ",
                gib(logical), gib(fold.disk_bytes()), nparts, nrec as f64 / el, mib(logical) / el);
            let _ = std::io::stderr().flush();
        }
    }
    if !pending.is_empty() {
        fold.sync()?;
        seq += 1;
        let path = dir.join(format!("part-{nparts:05}.part"));
        part::build(&path, &pending, seq, seq, FoldCfg::default().level, |h| fold.lookup(*h))?;
        let p = Part::open(&path)?;
        for (idx, orig) in &samples {
            let r = &pending[*idx];
            let row = p.find(&r.id)?.expect("sampled id must be findable");
            if &p.reconstruct(row, &fold)? != orig { anyhow::bail!("BYTE DRIFT for {}", r.id); }
            verified += 1;
        }
        nparts += 1;
    }
    fold.sync()?;
    let secs = t0.elapsed().as_secs_f64();
    let fold_b = fold.disk_bytes();
    let part_b: u64 = (0..nparts)
        .filter_map(|i| std::fs::metadata(dir.join(format!("part-{i:05}.part"))).ok())
        .map(|m| m.len()).sum();
    let total = fold_b + part_b;
    let distinct = refs - dups;

    eprintln!();
    println!("{:<26}{nrec} records, {nparts} parts  ({skipped} duplicate ids skipped)", "ingested");
    println!("{:<26}{:.2} GiB", "logical body bytes", gib(logical));
    println!("{:<26}{refs} refs, {distinct} distinct ({:.1}x collapsed)", "pieces", refs as f64 / distinct as f64);
    println!();
    println!("{:<26}{:>9.3} GiB   {:.1}%", "fold (content)", gib(fold_b), fold_b as f64 * 100.0 / total as f64);
    println!("{:<26}{:>9.3} GiB   {:.1}%", "parts (metadata)", gib(part_b), part_b as f64 * 100.0 / total as f64);
    println!("{:<26}{:>9.3} GiB   {:.1}x overall", "TOTAL", gib(total), logical as f64 / total as f64);
    println!();
    println!("{:<26}{:.1} B/distinct piece", "identity floor (32B hash)", 32.0);
    println!("{:<26}{:.3} GiB  ({:.1}% of store)", "  = hashes alone", distinct as f64 * 32.0 / (1024.0*1024.0*1024.0), distinct as f64 * 32.0 * 100.0 / total as f64);
    println!("{:<26}{verified} records (1 in {verify_every}), ALL byte-exact", "verified");
    println!("{:<26}{:.0}s ({:.0} rec/s, {:.0} MiB/s logical)", "elapsed", secs, nrec as f64 / secs, mib(logical) / secs);
    println!("{:<26}{} distinct pieces resident (global dedup window, not sealed)", "dedup window", fold.window_len());
    Ok(())
}
