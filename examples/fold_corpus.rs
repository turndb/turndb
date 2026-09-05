//! Measure the fold on a real gen_ai corpus, and prove byte-exact reconstruction on it.
//!
//! usage: fold_corpus <corpus.jsonl> [store-dir]
//!
//! Each line is a captured span; its `body` is a JSON array of messages. Agent traces replay the whole
//! conversation on every call, so message *k* recurs in every later call of the same session — the
//! prefix explosion the fold exists to collapse. We carve the body into one piece per top-level message
//! (byte-faithful spans, with the exact separators kept as inline literals), fold the pieces, then
//! reconstruct every record and compare against the original bytes.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Instant;
use turndb::fold::{Fold, FoldCfg};
use turndb::{ContentOp, PieceHash};

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

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let corpus = PathBuf::from(args.next().expect("usage: fold_corpus <corpus.jsonl> [store-dir]"));
    let dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("turndb-fold-corpus"));
    let d = FoldCfg::default();
    let block_target: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(d.block_target);
    let level: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(d.level);
    let _ = std::fs::remove_dir_all(&dir);

    let mut fold = Fold::open(&dir, FoldCfg { block_target, level, ..Default::default() })?;
    let f = std::fs::File::open(&corpus)?;
    let rdr = BufReader::with_capacity(1 << 20, f);

    let (mut records, mut logical, mut refs, mut dups, mut lit_bytes) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut verified = 0u64;
    // Decompose the ratio: dedup and compression are different wins and must be reported apart.
    // Compression is now per BLOCK, so a per-piece stored size no longer exists — the compressed
    // figure comes from the fold's durable bytes.
    let mut distinct_raw = 0u64;
    const BUCKETS: [(&str, u32); 6] = [
        ("<256B", 256),
        ("256B-1K", 1024),
        ("1K-4K", 4096),
        ("4K-16K", 16384),
        ("16K-64K", 65536),
        (">=64K", u32::MAX),
    ];
    let mut hist = [(0u64, 0u64); 6]; // (count, raw)
    let t0 = Instant::now();

    for line in rdr.lines() {
        let line = line?;
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let body = match v.get("body").and_then(|b| b.as_str()) {
            Some(b) => b.as_bytes().to_vec(),
            None => continue,
        };
        records += 1;
        logical += body.len() as u64;

        // carve -> flat program
        let mut prog: Vec<ContentOp> = Vec::new();
        for (foldable, r) in carve(&body) {
            let span = &body[r.clone()];
            if foldable {
                let p = fold.put(span)?;
                refs += 1;
                if p.deduped {
                    dups += 1;
                } else {
                    distinct_raw += p.loc.raw as u64;
                    let b = BUCKETS.iter().position(|(_, hi)| p.loc.raw < *hi).unwrap_or(5);
                    hist[b].0 += 1;
                    hist[b].1 += p.loc.raw as u64;
                }
                prog.push(ContentOp::Piece { hash: p.hash, len: span.len() as u32 });
            } else {
                lit_bytes += span.len() as u64;
                prog.push(ContentOp::Lit(span.to_vec()));
            }
        }

        // THE GATE: reconstruct from the fold and compare with the original bytes.
        let mut rebuilt = Vec::with_capacity(body.len());
        for op in &prog {
            match op {
                ContentOp::Lit(b) => rebuilt.extend_from_slice(b),
                ContentOp::Piece { hash, len } => {
                    // resolve through the same path a reader would use
                    let loc = fold_lookup(&fold, *hash).expect("piece just written must resolve");
                    let bytes = fold.read_verified(loc, *hash)?;
                    assert_eq!(bytes.len() as u32, *len);
                    rebuilt.extend_from_slice(&bytes);
                }
            }
        }
        if rebuilt != body {
            anyhow::bail!("BYTE DRIFT on record {records}");
        }
        verified += 1;

        if records % 5000 == 0 {
            print!("\r  {records} records…");
            let _ = std::io::stdout().flush();
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    fold.sync()?;
    let disk = fold.disk_bytes();
    let distinct = refs - dups;

    println!(
        "\r{:<28}{}  block={}K level={}",
        "corpus",
        corpus.display(),
        block_target / 1024,
        level
    );
    println!("{:<28}{records}", "records");
    println!("{:<28}{:.2} MiB", "logical body bytes", mib(logical));
    println!();
    println!("{:<28}{refs}", "piece references");
    println!(
        "{:<28}{distinct}  ({:.1}% of refs)",
        "distinct pieces",
        distinct as f64 * 100.0 / refs as f64
    );
    println!(
        "{:<28}{dups}  ({:.1}x amplification collapsed)",
        "duplicate hits",
        refs as f64 / distinct as f64
    );
    println!("{:<28}{:.2} MiB", "inline literal bytes", mib(lit_bytes));
    println!();
    println!("-- where the ratio comes from --");
    println!("{:<28}{:>10.2} MiB", "logical", mib(logical));
    println!(
        "{:<28}{:>10.2} MiB   {:.1}x  <- DEDUP (distinct pieces, uncompressed)",
        "after dedup",
        mib(distinct_raw),
        logical as f64 / distinct_raw as f64
    );
    println!(
        "{:<28}{:>10.2} MiB   {:.1}x  <- BLOCK COMPRESSION",
        "fold on disk",
        mib(disk),
        distinct_raw as f64 / disk as f64
    );
    println!(
        "{:<28}{:>10.2} MiB   {:.1}x  <- overall",
        "",
        mib(disk),
        logical as f64 / disk as f64
    );
    println!();
    println!("-- distinct piece sizes --");
    println!("{:<10}{:>9} {:>12}", "size", "pieces", "raw MiB");
    for (i, (name, _)) in BUCKETS.iter().enumerate() {
        let (c, r) = hist[i];
        if c == 0 {
            continue;
        }
        println!("{:<10}{:>9} {:>12.2}", name, c, mib(r));
    }
    let small: u64 = hist[0].1 + hist[1].1;
    println!(
        "\npieces under 1 KiB hold {:.2} MiB of {:.2} MiB distinct ({:.1}%) — blocking is what stops\nthem being penalised, since they no longer compress alone.",
        mib(small), mib(distinct_raw), small as f64 * 100.0 / distinct_raw as f64
    );
    let cs = fold.cache_stats();
    println!(
        "{:<28}{} hits / {} misses ({:.1}% hit)",
        "block cache",
        cs.hits,
        cs.misses,
        cs.hits as f64 * 100.0 / (cs.hits + cs.misses).max(1) as f64
    );
    println!("{:<28}{verified} records, ALL byte-exact", "verified");
    println!(
        "{:<28}{:.1}s  ({:.0} rec/s, {:.1} MiB/s logical)",
        "elapsed",
        secs,
        records as f64 / secs,
        mib(logical) / secs
    );
    Ok(())
}

/// The fold's own dedup window resolves a hash we just wrote. (A general hash->Loc lookup across published
/// parts arrives with the part layer; within one session everything is still in the window.)
fn fold_lookup(fold: &Fold, h: PieceHash) -> Option<turndb::fold::Loc> {
    fold.lookup(h)
}
