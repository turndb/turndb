//! Step-2 corpus gate: ingest a real gen_ai corpus into fold + parts, then verify every record
//! BYTE-EXACT by reading it back through the part.
//!
//! usage: fold_corpus <corpus.jsonl> [store-dir]
//!
//! Each line is a captured span; its `body` is a JSON array of messages. Agent traces replay the whole
//! conversation on every call, so message *k* recurs in every later call of the same session — the
//! prefix explosion the fold exists to collapse. We carve the body into one piece per top-level message
//! (byte-faithful spans, with the exact separators kept as inline literals), fold the pieces, then
//! reconstruct every record THROUGH THE PART and compare against the original bytes.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Instant;
use turndb::fold::{Fold, FoldCfg};
use turndb::part::{self, Part};
use turndb::{BodyOp, Record};

/// Byte ranges of the top-level elements of a JSON array, or None if this is not an array.
/// String- and escape-aware so a `[`, `]` or `,` inside a string never splits an element.
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
    None // unterminated
}

/// Carve a body into a flat program: literals for the structural glue, pieces for the messages.
fn carve(body: &[u8]) -> Vec<(bool, std::ops::Range<usize>)> {
    match split_json_array(body) {
        Some(elems) if !elems.is_empty() => {
            let mut out = Vec::with_capacity(elems.len() * 2 + 1);
            let mut cur = 0usize;
            for (a, b) in elems {
                if a > cur {
                    out.push((false, cur..a)); // glue: '[', ', ', whitespace
                }
                out.push((true, a..b)); // a message — foldable
                cur = b;
            }
            if cur < body.len() {
                out.push((false, cur..body.len()));
            }
            out
        }
        _ => vec![(true, 0..body.len())], // not an array: fold the whole body
    }
}


fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

const PART_RECORDS: usize = 5000;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let corpus = PathBuf::from(args.next().expect("usage: part_corpus <corpus.jsonl> [dir]"));
    let dir = args.next().map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("turndb-part-corpus"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let mut fold = Fold::open(&dir.join("fold"), FoldCfg::default())?;
    let rdr = BufReader::with_capacity(1 << 20, std::fs::File::open(&corpus)?);

    let (mut nrec, mut logical, mut refs, mut dups) = (0u64, 0u64, 0u64, 0u64);
    let mut pending: Vec<Record> = Vec::new();
    let mut originals: Vec<Vec<u8>> = Vec::new();
    let mut parts: Vec<PathBuf> = Vec::new();
    let mut seq = 0u64;
    let mut verified = 0u64;
    let t0 = Instant::now();

    let flush = |fold: &mut Fold,
                     pending: &mut Vec<Record>,
                     originals: &mut Vec<Vec<u8>>,
                     parts: &mut Vec<PathBuf>,
                     seq: &mut u64,
                     verified: &mut u64| -> anyhow::Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        // data before pointers: the fold is durable before a part names any of it
        fold.sync()?;
        *seq += 1;
        let path = dir.join(format!("part-{:05}.part", parts.len()));
        part::build(&path, pending, *seq, *seq, FoldCfg::default().level, |h| fold.lookup(*h))?;
        let p = Part::open(&path)?;
        // THE GATE: read every record back through the part and compare with the original bytes
        for (r, orig) in pending.iter().zip(originals.iter()) {
            let row = p.find(&r.id)?.expect("id must be findable in the part it was built from");
            let got = p.reconstruct(row, fold)?;
            if &got != orig {
                anyhow::bail!("BYTE DRIFT for {}", r.id);
            }
            if p.attrs(row)? != r.attrs {
                anyhow::bail!("ATTR DRIFT for {}", r.id);
            }
            *verified += 1;
        }
        parts.push(path);
        pending.clear();
        originals.clear();
        Ok(())
    };

    for line in rdr.lines() {
        let line = line?;
        let v: serde_json::Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
        let body = match v.get("body").and_then(|b| b.as_str()) { Some(b) => b.as_bytes().to_vec(), None => continue };
        let id = format!(
            "{}:{}#{}",
            v.get("trace_id").and_then(|x| x.as_str()).unwrap_or("t"),
            v.get("span_id").and_then(|x| x.as_str()).unwrap_or("s"),
            v.get("kind").and_then(|x| x.as_str()).unwrap_or("k")
        );
        // attrs: every scalar field except the body
        let mut attrs = Vec::new();
        if let Some(obj) = v.as_object() {
            for (k, val) in obj {
                if k == "body" { continue; }
                let av = match val {
                    serde_json::Value::String(s) => turndb::AttrValue::Str(s.clone()),
                    serde_json::Value::Bool(b) => turndb::AttrValue::Bool(*b),
                    serde_json::Value::Number(n) if n.is_i64() => turndb::AttrValue::Int(n.as_i64().unwrap()),
                    serde_json::Value::Number(n) => turndb::AttrValue::Float(n.as_f64().unwrap_or(0.0)),
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
        // ids must be unique within a part; skip an exact repeat rather than fail the run
        if pending.iter().any(|r| r.id == id) {
            continue;
        }
        pending.push(Record { id, body: prog, attrs });
        originals.push(body);
        if pending.len() >= PART_RECORDS {
            flush(&mut fold, &mut pending, &mut originals, &mut parts, &mut seq, &mut verified)?;
            print!("\r  {nrec} records, {} parts…", parts.len());
            let _ = std::io::stdout().flush();
        }
    }
    flush(&mut fold, &mut pending, &mut originals, &mut parts, &mut seq, &mut verified)?;
    let secs = t0.elapsed().as_secs_f64();

    let fold_bytes = fold.disk_bytes();
    let part_bytes: u64 = parts.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    let total = fold_bytes + part_bytes;

    println!("\r{:<26}{}", "corpus", corpus.display());
    println!("{:<26}{nrec} records, {} parts", "ingested", parts.len());
    println!("{:<26}{:.2} MiB", "logical body bytes", mib(logical));
    println!("{:<26}{refs} refs, {} distinct ({:.1}x collapsed)", "pieces", refs - dups, refs as f64 / (refs - dups) as f64);
    println!();
    println!("{:<26}{:>9.2} MiB   {:.1}%", "fold (content)", mib(fold_bytes), fold_bytes as f64 * 100.0 / total as f64);
    println!("{:<26}{:>9.2} MiB   {:.1}%", "parts (metadata)", mib(part_bytes), part_bytes as f64 * 100.0 / total as f64);
    println!("{:<26}{:>9.2} MiB   {:.1}x overall", "TOTAL", mib(total), logical as f64 / total as f64);
    println!();
    println!("{:<26}{verified} records, ALL byte-exact through the part", "verified");
    println!("{:<26}{:.1}s ({:.0} rec/s)", "elapsed", secs, nrec as f64 / secs);
    Ok(())
}
