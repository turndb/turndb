//! When does content addressing pay for its hashes?
//!
//! usage: dedup_math <corpus.jsonl> [field]
//!
//! Dedup costs 32 bytes of BLAKE3 per distinct piece, plus a location, and buys not storing the
//! duplicates. But the competitor is not "store everything uncompressed" — it is zstd, which already
//! finds repeats inside its own window for free. So content addressing only earns its hashes on
//! duplication zstd CANNOT see: repeats separated by more than a block, or arriving days apart.
//!
//! This measures both sides on a real corpus and reports where the break-even sits.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Instant;

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


fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

/// Block-compress a byte stream with NO dedup — the honest competitor. The block size IS the window
/// zstd can match within, so this sweeps how much duplication zstd catches on its own.
fn blocked(all: &[Vec<u8>], block: usize, level: i32) -> u64 {
    let (mut total, mut buf) = (0u64, Vec::with_capacity(block * 2));
    for b in all {
        buf.extend_from_slice(b);
        if buf.len() >= block {
            total += zstd::bulk::compress(&buf, level).unwrap().len() as u64;
            buf.clear();
        }
    }
    if !buf.is_empty() {
        total += zstd::bulk::compress(&buf, level).unwrap().len() as u64;
    }
    total
}

fn main() -> anyhow::Result<()> {
    let corpus = PathBuf::from(std::env::args().nth(1).expect("usage: dedup_math <corpus.jsonl> [field]"));
    let field = std::env::args().nth(2).unwrap_or_else(|| "body".to_string());

    let mut index: HashMap<[u8; 32], u32> = HashMap::new();
    let mut pieces: Vec<Vec<u8>> = Vec::new();
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let (mut logical, mut refs) = (0u64, 0u64);
    let rdr = BufReader::with_capacity(1 << 20, std::fs::File::open(&corpus)?);
    for line in rdr.lines() {
        let line = line?;
        let v: serde_json::Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
        let body = match v.get(&field).and_then(|b| b.as_str()) { Some(b) => b.as_bytes().to_vec(), None => continue };
        logical += body.len() as u64;
        let spans = match split_json_array(&body) { Some(e) if !e.is_empty() => e, _ => vec![(0, body.len())] };
        for (a, b) in spans {
            let span = &body[a..b];
            refs += 1;
            let h: [u8; 32] = blake3::hash(span).into();
            index.entry(h).or_insert_with(|| { pieces.push(span.to_vec()); (pieces.len() - 1) as u32 });
        }
        bodies.push(body);
    }
    let d_count = pieces.len() as u64;
    let raw_distinct: u64 = pieces.iter().map(|p| p.len() as u64).sum();
    let dup = refs as f64 / d_count as f64;
    let s = raw_distinct as f64 / d_count as f64;

    println!("corpus            {}", corpus.display());
    println!("logical           {:.2} MiB over {} bodies", mib(logical), bodies.len());
    println!("references N      {refs}");
    println!("distinct    D     {d_count}");
    println!("duplication d=N/D {dup:.1}x");
    println!("mean piece  s     {s:.0} B\n");

    // ---- Design A: content addressed. Fold = distinct pieces, block compressed. ----
    println!("-- A: content-addressed (distinct pieces block-compressed) --");
    println!("{:<14}{:>12}{:>14}{:>16}", "block/level", "fold MiB", "B/distinct p", "+identity h");
    let mut a_fold = 0u64;
    for (blk, lvl) in [(4 << 20, 19)] {
        a_fold = blocked(&pieces, blk, lvl);
        let p = a_fold as f64 / d_count as f64;
        println!("{:<14}{:>12.2}{:>14.1}{:>16.1}", format!("{}M/z{lvl}", blk >> 20), mib(a_fold), p, p + 37.1);
    }

    // ---- Design B: no dedup. Every occurrence stored; zstd finds what it can within the window. ----
    println!("\n-- B: no content addressing (all bodies block-compressed; block size = zstd's window) --");
    println!("{:<14}{:>12}{:>12}{:>16}", "block/level", "stored MiB", "overall", "vs A total");
    let a_total = a_fold as f64 + 2.22 * 1048576.0; // measured part metadata for this corpus
    for (blk, lvl) in [(4usize << 20, 19), (16 << 20, 19), (64 << 20, 19), (256 << 20, 19)] {
        let b = blocked(&bodies, blk, lvl);
        println!(
            "{:<14}{:>12.2}{:>11.1}x{:>15.2}x",
            format!("{}M/z{lvl}", blk >> 20), mib(b), logical as f64 / b as f64, b as f64 / a_total
        );
    }
    println!("\nA total (fold + measured part metadata) = {:.2} MiB", a_total / 1048576.0);

    // ---- break-even ----
    let p = a_fold as f64 / d_count as f64;
    let h = 37.1f64; // 32B hash + ~5.1B stored loc, measured
    let r = 0.19f64; // stored bytes per reference, measured
    println!("\n-- break-even --");
    println!("  d* = (p + h) / (s/c' - r),  where p = compressed bytes per distinct piece");
    println!("  when zstd cannot see the duplicates (s/c' -> p):   d* ~= 1 + h/p");
    println!("  here p = {p:.1} B, h = {h:.1} B  =>  d* = {:.2}x", 1.0 + h / (p - r));
    println!("  this corpus runs at d = {dup:.1}x  ({:.0}x past break-even)", dup / (1.0 + h / (p - r)));
    println!("\n  break-even by piece size (h = {h:.0} B):");
    println!("  {:>14}{:>12}", "compressed p", "d* needed");
    for pp in [8.0, 16.0, 37.0, 64.0, 116.0, 256.0, 1024.0] {
        println!("  {:>14.0}{:>11.2}x", pp, 1.0 + h / pp);
    }
    Ok(())
}
