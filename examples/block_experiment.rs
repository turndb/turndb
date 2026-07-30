//! Per-piece framing vs block framing: what does read-at-ratio actually cost?
//!
//! usage: block_experiment <corpus.jsonl>
//!
//! The fold frames every piece individually so a point read decompresses one piece. The alternative —
//! grouping pieces into blocks and compressing the block — exploits cross-piece redundancy and stops
//! penalising small pieces, but a point read then costs a whole block.
//!
//! This measures both axes (framing × level) on the real distinct-piece set, plus the point-read cost
//! each scheme implies, so the choice rests on numbers instead of doctrine.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Instant;

// ---- carve (same rule as fold_corpus: one piece per top-level JSON array element) ----

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

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

/// Where a piece landed: `(block index, offset within it, length)`.
type PiecePlacement = (usize, usize, usize);

fn main() -> anyhow::Result<()> {
    let corpus =
        PathBuf::from(std::env::args().nth(1).expect("usage: block_experiment <corpus.jsonl>"));

    // Collect the DISTINCT pieces in fold insertion order — which is capture order, so temporally
    // adjacent pieces (same session, same task) land near each other. That ordering is what any
    // blocking scheme would actually see.
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut pieces: Vec<Vec<u8>> = Vec::new();
    let mut logical = 0u64;
    let rdr = BufReader::with_capacity(1 << 20, std::fs::File::open(&corpus)?);
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
        logical += body.len() as u64;
        let spans = match split_json_array(&body) {
            Some(e) if !e.is_empty() => e,
            _ => vec![(0, body.len())],
        };
        for (a, b) in spans {
            let span = &body[a..b];
            let h: [u8; 32] = blake3::hash(span).into();
            if seen.insert(h) {
                pieces.push(span.to_vec());
            }
        }
    }
    let raw: u64 = pieces.iter().map(|p| p.len() as u64).sum();
    println!("corpus            {}", corpus.display());
    println!("logical           {:.2} MiB", mib(logical));
    println!("distinct pieces   {}  ({:.2} MiB raw)", pieces.len(), mib(raw));
    println!();

    // ---- A. per-piece framing (what the fold does today) ----
    println!("-- per-piece framing (read 1 piece = decompress 1 piece) --");
    println!("{:<12}{:>12}{:>10}{:>12}{:>14}", "level", "size MiB", "ratio", "overall", "compress");
    let mut per_piece: Vec<(i32, u64)> = Vec::new();
    for lvl in [3, 19] {
        let t = Instant::now();
        let total: u64 =
            pieces.iter().map(|p| zstd::bulk::compress(p, lvl).unwrap().len() as u64).sum();
        let secs = t.elapsed().as_secs_f64();
        // + 16 B framing per piece, as the fold actually writes it
        let on_disk = total + 16 * pieces.len() as u64;
        per_piece.push((lvl, on_disk));
        println!(
            "{:<12}{:>12.2}{:>9.1}x{:>11.1}x{:>13.1}s",
            format!("zstd-{lvl}"),
            mib(on_disk),
            raw as f64 / on_disk as f64,
            logical as f64 / on_disk as f64,
            secs
        );
    }

    // ---- B. block framing ----
    println!();
    println!("-- block framing (read 1 piece = decompress its whole block) --");
    println!(
        "{:<12}{:>12}{:>10}{:>12}{:>10}{:>12}",
        "scheme", "size MiB", "ratio", "overall", "blocks", "compress"
    );
    let mut best: Option<(String, u64, usize)> = None;
    for block_bytes in [64 * 1024usize, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024] {
        for lvl in [3, 19] {
            let t = Instant::now();
            let (mut total, mut nblocks) = (0u64, 0usize);
            let mut buf: Vec<u8> = Vec::with_capacity(block_bytes * 2);
            for p in &pieces {
                buf.extend_from_slice(p);
                if buf.len() >= block_bytes {
                    total += zstd::bulk::compress(&buf, lvl).unwrap().len() as u64;
                    nblocks += 1;
                    buf.clear();
                }
            }
            if !buf.is_empty() {
                total += zstd::bulk::compress(&buf, lvl).unwrap().len() as u64;
                nblocks += 1;
            }
            let secs = t.elapsed().as_secs_f64();
            // per piece: a varint-ish (block ordinal, offset, len) entry; per block: an offset entry
            let index = pieces.len() as u64 * 10 + nblocks as u64 * 8;
            let on_disk = total + index;
            let name = format!("{}K/z{lvl}", block_bytes / 1024);
            println!(
                "{:<12}{:>12.2}{:>9.1}x{:>11.1}x{:>10}{:>11.1}s",
                name,
                mib(on_disk),
                raw as f64 / on_disk as f64,
                logical as f64 / on_disk as f64,
                nblocks,
                secs
            );
            if best.as_ref().is_none_or(|(_, b, _)| on_disk < *b) {
                best = Some((name, on_disk, block_bytes));
            }
        }
    }

    // ---- C. what a point read costs in each scheme ----
    println!();
    println!("-- point-read cost (decompress path for ONE piece) --");
    let sample: Vec<usize> = (0..2000).map(|i| (i * 7919) % pieces.len()).collect();

    let comp3: Vec<Vec<u8>> = pieces.iter().map(|p| zstd::bulk::compress(p, 3).unwrap()).collect();
    let t = Instant::now();
    for &i in &sample {
        let out = zstd::bulk::decompress(&comp3[i], pieces[i].len().max(1)).unwrap();
        std::hint::black_box(out);
    }
    let per_piece_us = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;
    println!("per-piece zstd-3        {per_piece_us:>8.1} us/read");

    if let Some((_, _, bb)) = &best {
        // rebuild blocks at the winning size, record which block each piece landed in
        let (mut blocks, mut where_of): (Vec<Vec<u8>>, Vec<PiecePlacement>) =
            (Vec::new(), Vec::new());
        let mut buf: Vec<u8> = Vec::new();
        for p in &pieces {
            let off = buf.len();
            buf.extend_from_slice(p);
            where_of.push((blocks.len(), off, p.len()));
            if buf.len() >= *bb {
                blocks.push(std::mem::take(&mut buf));
            }
        }
        if !buf.is_empty() {
            blocks.push(buf);
        }
        for lvl in [3, 19] {
            let cblocks: Vec<Vec<u8>> =
                blocks.iter().map(|b| zstd::bulk::compress(b, lvl).unwrap()).collect();
            let t = Instant::now();
            for &i in &sample {
                let (bi, off, len) = where_of[i];
                let whole = zstd::bulk::decompress(&cblocks[bi], blocks[bi].len()).unwrap();
                std::hint::black_box(&whole[off..off + len]);
            }
            let us = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;
            println!(
                "block {:>4}K zstd-{lvl:<2}      {us:>8.1} us/read   ({:.0}x the per-piece cost)",
                bb / 1024,
                us / per_piece_us
            );
        }
    }
    Ok(())
}
