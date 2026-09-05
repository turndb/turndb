//! Parallel large-corpus ingest: N reader threads parse/carve/hash, one thread appends.
//!
//! usage: swe_par <dir> <fifo1> [fifo2 ...] [--per-part N] [--verify-every N]
//!
//! The serial ingest was using ~1.2 of 22 cores: a single Python producer and a single Rust consumer
//! taking turns through a pipe, both spending their time in JSON parsing. Parsing, carving and
//! hashing are per-record and share nothing, so they fan out; only the fold's append point is
//! genuinely serial, and once hashing has already happened it does little more than a hash-table
//! probe and a memcpy.
//!
//! Records therefore arrive in nondeterministic order, so the fold's physical layout depends on
//! scheduling. Content, identity and reconstruction are unaffected — only byte-identity of the fold
//! between two runs of the same input is given up, which is the right trade for an ingest harness.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::mpsc::sync_channel;
use std::time::Instant;
use turndb::fold::{Fold, FoldCfg};
use turndb::part::{self, Part};
use turndb::{AttrValue, ContentOp, PieceHash, Record};

fn split_json_array(s: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut i = 0;
    while i < s.len() && (s[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    if i >= s.len() || s[i] != b'[' {
        return None;
    }
    i += 1;
    let mut out = Vec::new();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    let mut start: Option<usize> = None;
    while i < s.len() {
        let c = s[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => {
                    in_str = true;
                    if start.is_none() {
                        start = Some(i);
                    }
                }
                b'[' | b'{' => {
                    if start.is_none() {
                        start = Some(i);
                    }
                    depth += 1;
                }
                b']' | b'}' => {
                    if depth == 0 && c == b']' {
                        if let Some(st) = start.take() {
                            out.push((st, i));
                        }
                        return Some(out);
                    }
                    depth -= 1;
                }
                b',' if depth == 0 => {
                    if let Some(st) = start.take() {
                        out.push((st, i));
                    }
                }
                w if (w as char).is_ascii_whitespace() => {}
                _ => {
                    if start.is_none() {
                        start = Some(i);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// An op whose expensive work (hashing) is already done; the serial stage only places it.
enum PreOp {
    Lit(Range<usize>),
    Piece { hash: PieceHash, at: Range<usize> },
}

/// Everything a worker can produce without touching shared state.
struct Prepared {
    id: String,
    body: Vec<u8>,
    ops: Vec<PreOp>,
    attrs: Vec<(String, AttrValue)>,
}

fn prepare(line: &str) -> Option<Prepared> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let body = v.get("body")?.as_str()?.as_bytes().to_vec();
    let id = format!(
        "{}:{}#{}",
        v.get("trace_id").and_then(|x| x.as_str()).unwrap_or("t"),
        v.get("span_id").and_then(|x| x.as_str()).unwrap_or("s"),
        v.get("kind").and_then(|x| x.as_str()).unwrap_or("k")
    );
    let mut attrs = Vec::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if k == "body" {
                continue;
            }
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
    // carve + hash — the expensive, parallel part
    let spans = match split_json_array(&body) {
        Some(e) if !e.is_empty() => e,
        _ => vec![(0, body.len())],
    };
    let mut ops = Vec::with_capacity(spans.len() * 2 + 1);
    let mut cur = 0usize;
    for (a, b) in spans {
        if a > cur {
            ops.push(PreOp::Lit(cur..a));
        }
        ops.push(PreOp::Piece { hash: PieceHash::of(&body[a..b]), at: a..b });
        cur = b;
    }
    if cur < body.len() {
        ops.push(PreOp::Lit(cur..body.len()));
    }
    Some(Prepared { id, body, ops, attrs })
}

fn gib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0 * 1024.0)
}
fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn main() -> anyhow::Result<()> {
    let mut fifos: Vec<PathBuf> = Vec::new();
    let mut dir: Option<PathBuf> = None;
    let (mut per_part, mut verify_every) = (20_000usize, 500usize);
    let mut level: i32 = FoldCfg::default().level;
    let mut part_level: i32 = 0; // 0 = same as fold
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--per-part" => per_part = it.next().and_then(|s| s.parse().ok()).unwrap_or(per_part),
            "--verify-every" => {
                verify_every = it.next().and_then(|s| s.parse().ok()).unwrap_or(verify_every)
            }
            "--level" => level = it.next().and_then(|s| s.parse().ok()).unwrap_or(level),
            "--part-level" => {
                part_level = it.next().and_then(|s| s.parse().ok()).unwrap_or(part_level)
            }
            _ if dir.is_none() => dir = Some(PathBuf::from(a)),
            _ => fifos.push(PathBuf::from(a)),
        }
    }
    let dir = dir.expect("usage: swe_par <dir> <fifo...>");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let plevel = if part_level == 0 { level } else { part_level };
    eprintln!(
        "readers: {}  per-part: {per_part}  fold-level: {level}  part-level: {plevel}",
        fifos.len()
    );

    // Backpressure keeps memory bounded: workers block once the append stage falls behind.
    let (tx, rx) = sync_channel::<Prepared>(4096);
    let mut handles = Vec::new();
    for f in fifos {
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            let file = std::fs::File::open(&f).expect("open fifo");
            let rdr = BufReader::with_capacity(1 << 22, file);
            for line in rdr.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.is_empty() {
                    continue;
                }
                if let Some(p) = prepare(&line) {
                    if tx.send(p).is_err() {
                        break;
                    }
                }
            }
        }));
    }
    drop(tx);

    let mut fold = Fold::open(&dir.join("fold"), FoldCfg { level, ..Default::default() })?;
    let (mut nrec, mut logical, mut refs, mut dups, mut skipped) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut pending, mut samples): (Vec<Record>, Vec<(usize, Vec<u8>)>) = (Vec::new(), Vec::new());
    let mut seen: HashSet<String> = HashSet::new();
    let (mut nparts, mut seq, mut verified) = (0usize, 0u64, 0u64);
    let t0 = Instant::now();

    for p in rx {
        if seen.contains(&p.id) {
            skipped += 1;
            continue;
        }
        seen.insert(p.id.clone());
        nrec += 1;
        logical += p.body.len() as u64;
        let mut prog = Vec::with_capacity(p.ops.len());
        for op in &p.ops {
            match op {
                PreOp::Lit(r) => prog.push(ContentOp::Lit(p.body[r.clone()].to_vec())),
                PreOp::Piece { hash, at } => {
                    // the only serialized work: probe + (on a miss) a memcpy into the open block
                    let put = fold.put_hashed(&p.body[at.clone()], *hash)?;
                    refs += 1;
                    if put.deduped {
                        dups += 1;
                    }
                    prog.push(ContentOp::Piece { hash: *hash, len: (at.end - at.start) as u32 });
                }
            }
        }
        let sample_this = nrec % verify_every as u64 == 0;
        let Prepared { id, body, attrs, .. } = p;
        if sample_this {
            samples.push((pending.len(), body));
        }
        pending.push(Record {
            id,
            contents: vec![turndb::Content::new(turndb::BODY_CONTENT, prog)],
            attrs,
        });

        if pending.len() >= per_part {
            fold.sync()?;
            seq += 1;
            let path = dir.join(format!("part-{nparts:05}.part"));
            part::build(&path, &pending, seq, seq, plevel, |h| fold.lookup(*h))?;
            let pt = Part::open(&path)?;
            for (idx, orig) in &samples {
                let r = &pending[*idx];
                let row = pt.find(&r.id)?.expect("sampled id findable");
                if &pt.reconstruct(row, &fold)? != orig {
                    anyhow::bail!("BYTE DRIFT for {}", r.id);
                }
                if pt.attrs(row)? != r.attrs {
                    anyhow::bail!("ATTR DRIFT for {}", r.id);
                }
                verified += 1;
            }
            nparts += 1;
            pending.clear();
            samples.clear();
            seen.clear();
            let el = t0.elapsed().as_secs_f64();
            eprint!("\r  {nrec} rec | {:.1} GiB | fold {:.2} GiB | {} parts | {:.0} rec/s | {:.0} MiB/s   ",
                gib(logical), gib(fold.disk_bytes()), nparts, nrec as f64 / el, mib(logical) / el);
            let _ = std::io::stderr().flush();
        }
    }
    if !pending.is_empty() {
        fold.sync()?;
        seq += 1;
        let path = dir.join(format!("part-{nparts:05}.part"));
        part::build(&path, &pending, seq, seq, plevel, |h| fold.lookup(*h))?;
        nparts += 1;
    }
    for h in handles {
        let _ = h.join();
    }
    fold.sync()?;
    let secs = t0.elapsed().as_secs_f64();
    let fold_b = fold.disk_bytes();
    let part_b: u64 = (0..nparts)
        .filter_map(|i| std::fs::metadata(dir.join(format!("part-{i:05}.part"))).ok())
        .map(|m| m.len())
        .sum();
    let total = fold_b + part_b;
    let distinct = refs - dups;

    eprintln!();
    println!("{:<26}{nrec} records, {nparts} parts  ({skipped} duplicate ids skipped)", "ingested");
    println!("{:<26}{:.2} GiB", "logical body bytes", gib(logical));
    println!(
        "{:<26}{refs} refs, {distinct} distinct ({:.1}x collapsed)",
        "pieces",
        refs as f64 / distinct as f64
    );
    println!();
    println!(
        "{:<26}{:>9.3} GiB   {:.1}%",
        "fold (content)",
        gib(fold_b),
        fold_b as f64 * 100.0 / total as f64
    );
    println!(
        "{:<26}{:>9.3} GiB   {:.1}%",
        "parts (metadata)",
        gib(part_b),
        part_b as f64 * 100.0 / total as f64
    );
    println!(
        "{:<26}{:>9.3} GiB   {:.1}x overall",
        "TOTAL",
        gib(total),
        logical as f64 / total as f64
    );
    println!();
    println!(
        "{:<26}{:.3} GiB  ({:.1}% of store)",
        "hashes alone (32B)",
        distinct as f64 * 32.0 / (1024.0 * 1024.0 * 1024.0),
        distinct as f64 * 32.0 * 100.0 / total as f64
    );
    println!("{:<26}{verified} records (1 in {verify_every}), ALL byte-exact", "verified");
    println!(
        "{:<26}{:.0}s ({:.0} rec/s, {:.0} MiB/s logical)",
        "elapsed",
        secs,
        nrec as f64 / secs,
        mib(logical) / secs
    );
    println!("{:<26}{} distinct pieces resident", "dedup window", fold.window_len());
    Ok(())
}
