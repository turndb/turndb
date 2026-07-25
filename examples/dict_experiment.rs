//! Do trained dictionaries earn their place, and at what block size?
//!
//! The fold's format already carries everything a trained dictionary needs — a codec tag, a `dict_id`
//! in the segment header, a dictionary map, a pool that accepts one — and nothing trains one. Before
//! building that, the question worth answering is whether it would pay.
//!
// RESULT (SWE-rebench, 40,004 carved pieces, 71.1 MiB distinct):
//
//   block     no dict    trained     delta
//    4 KiB    16.458     10.370    +36.99%
//   64 KiB    10.496      8.193    +21.94%
//  256 KiB     8.678      7.548    +13.02%
//    1 MiB     7.545      7.331     +2.84%
//    4 MiB     6.943      7.224     -4.06%   <- the size actually in use
//   16 MiB     6.589      7.172     -8.85%
//
//! A trained dictionary is not merely useless at the block size in use — it is HARMFUL. It supplies
//! context a compressor lacks, and a 4 MiB block already holds megabytes of the same corpus, so the
//! dictionary spends window telling zstd what it can already see. Dictionaries earn their keep when
//! the compression unit is small, which is the regime the per-piece framing this engine rejected
//! would have been in. The crossover is around 1 MiB.
//!
//! # The measurement error that nearly inverted this
//!
//! A first pass deduped WHOLE BODIES and reported the dictionary winning 16% at 4 MiB. Bodies are full
//! conversation prefixes, so "distinct" bodies still share nearly everything; the ratio it measured
//! was that shared prefix being rediscovered, not compression of the units the fold stores. Carving
//! first — as the ingest path does — reversed the sign of the answer and brought it into agreement
//! with `block_curve`. The unit of measurement WAS the experiment.
//!
//! usage: <producer emitting JSONL with a `body` field> | dict_experiment [max-mib]

use std::io::{BufRead, BufReader};

/// Carve a JSON array body into its elements, exactly as the ingest path does.
///
/// This matters more than it looks. Deduping WHOLE BODIES instead measures the wrong thing entirely:
/// each body is a full conversation prefix, so "distinct" bodies still share almost everything with
/// one another, and the apparent compression ratio is really that shared prefix being found again.
/// The fold stores carved spans, so the experiment must too.
fn carve(body: &[u8]) -> Vec<(usize, usize)> {
    let mut i = 0;
    while i < body.len() && (body[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    if i >= body.len() || body[i] != b'[' {
        return vec![(0, body.len())];
    }
    i += 1;
    let (mut depth, mut instr, mut esc) = (0i32, false, false);
    let mut start: Option<usize> = None;
    let mut out = Vec::new();
    while i < body.len() {
        let c = body[i];
        if instr {
            if esc { esc = false } else if c == b'\\' { esc = true } else if c == b'"' { instr = false }
        } else {
            match c {
                b'"' => { instr = true; if start.is_none() { start = Some(i) } }
                b'{' | b'[' => { depth += 1; if start.is_none() { start = Some(i) } }
                b'}' | b']' => {
                    if depth == 0 { if let Some(st) = start.take() { out.push((st, i)); } break }
                    depth -= 1;
                }
                b',' if depth == 0 => { if let Some(st) = start.take() { out.push((st, i)); } }
                w if (w as char).is_ascii_whitespace() => {}
                _ => { if start.is_none() { start = Some(i) } }
            }
        }
        i += 1;
    }
    if out.is_empty() { vec![(0, body.len())] } else { out }
}

fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn blocks_of(content: &[Vec<u8>], target: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut cur = Vec::with_capacity(target);
    for c in content {
        cur.extend_from_slice(c);
        if cur.len() >= target {
            out.push(std::mem::take(&mut cur));
            cur.reserve(target);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn main() -> anyhow::Result<()> {
    let cap_mib: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(512);
    let cap = cap_mib * 1024 * 1024;

    // Pieces in CAPTURE ORDER, deduped — exactly what the fold would hold.
    let mut seen = std::collections::HashSet::new();
    let mut pieces: Vec<Vec<u8>> = Vec::new();
    let mut logical = 0usize;
    let mut stored_raw = 0usize;
    let rdr = BufReader::with_capacity(1 << 22, std::io::stdin().lock());
    for line in rdr.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let Some(body) = v.get("body").and_then(|b| b.as_str()) else { continue };
        logical += body.len();
        let b = body.as_bytes();
        for (a, z) in carve(b) {
            let span = &b[a..z];
            if seen.insert(*blake3::hash(span).as_bytes()) {
                stored_raw += span.len();
                pieces.push(span.to_vec());
            }
        }
        if stored_raw >= cap {
            break;
        }
    }
    println!(
        "{} distinct pieces, {:.1} MiB distinct out of {:.1} MiB logical\n",
        pieces.len(), mib(stored_raw), mib(logical)
    );

    // Train on a stratified sample so the dictionary is not just the first few pieces.
    let step = (pieces.len() / 8000).max(1);
    let samples: Vec<&[u8]> = pieces.iter().step_by(step).map(|p| p.as_slice()).take(8000)
        .map(|p| &p[..p.len().min(16 << 10)]).collect();
    println!("training on {} sampled pieces...", samples.len());
    let dict = zstd::dict::from_samples(&samples, 112 << 10)?;
    println!("dictionary {:.1} KiB\n", dict.len() as f64 / 1024.0);

    println!("{:<14}{:>10}{:>14}{:>14}{:>12}{:>10}",
        "block target", "blocks", "no dict MiB", "trained MiB", "delta", "ratio");

    for target in [4 << 10, 64 << 10, 256 << 10, 1 << 20, 4 << 20, 16 << 20] {
        let bs = blocks_of(&pieces, target);
        let mut plain = 0usize;
        let mut trained = 0usize;
        for b in &bs {
            plain += zstd::bulk::compress(b, 19)?.len();
            let mut c = zstd::bulk::Compressor::with_dictionary(19, &dict)?;
            trained += c.compress(b)?.len();
        }
        let d = plain as f64 - trained as f64;
        println!(
            "{:<14}{:>10}{:>14.3}{:>14.3}{:>11.2}%{:>9.2}x",
            format!("{} KiB", target / 1024), bs.len(), mib(plain), mib(trained),
            d * 100.0 / plain as f64,
            stored_raw as f64 / plain as f64
        );
    }
    println!("\ndelta > 0 means the trained dictionary helped.");
    Ok(())
}
