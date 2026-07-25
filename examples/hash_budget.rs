//! What the piece-hash column costs, and what truncating it would actually buy.
//!
//! `pdict.hash` is the largest single section in a part and it does not compress — BLAKE3 output is
//! uniform by construction, so a section that DID compress would mean the hashes were badly derived.
//! It is therefore the one part of the store that cannot be made smaller by encoding, only by storing
//! fewer bits. This measures what fewer bits would be worth, so the tradeoff is made against numbers.
//!
//! usage: hash_budget <dir-with-parts>

use std::collections::HashSet;
use std::path::PathBuf;
use turndb::part::Part;

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

/// P(at least one collision) among `n` uniform values of `bits` bits, via the birthday bound
/// n^2 / 2^(bits+1). Computed in logs — the direct form overflows immediately.
fn collision_log2(n: f64, bits: f64) -> f64 {
    2.0 * n.log2() - (bits + 1.0)
}

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: hash_budget <dir>"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .collect();
    paths.sort();

    let mut all: Vec<[u8; 32]> = Vec::new();
    let mut distinct: HashSet<[u8; 32]> = HashSet::new();
    let (mut part_bytes, mut hash_stored) = (0u64, 0u64);
    let mut records = 0usize;

    for p in &paths {
        let part = Part::open(p)?;
        records += part.len();
        part_bytes += std::fs::metadata(p)?.len();
        for (name, stored, _, _) in part.sections() {
            if name == "pdict.hash" {
                hash_stored += stored as u64;
            }
        }
        for i in 0..part.piece_count()? {
            let (_, h) = part.piece(i)?;
            all.push(h.0);
            distinct.insert(h.0);
        }
    }

    println!("{} parts, {records} records, {:.2} MiB of parts", paths.len(), mib(part_bytes));
    println!(
        "dictionary entries {} across parts, {} distinct pieces ({:.2}x carried more than once)\n",
        all.len(), distinct.len(), all.len() as f64 / distinct.len().max(1) as f64
    );

    // What the column costs today, and at narrower widths, MEASURED rather than assumed: compress each
    // truncation at the same level the part used and read off the real size.
    println!("{:<10}{:>12}{:>12}{:>10}{:>16}", "width", "raw MiB", "stored MiB", "ratio", "vs 32B");
    let mut base = 0f64;
    for w in [32usize, 24, 16, 12, 8] {
        let raw: Vec<u8> = all.iter().flat_map(|h| h[..w].to_vec()).collect();
        let stored = zstd::bulk::compress(&raw, 19)?;
        let s = stored.len() as f64;
        if w == 32 {
            base = s;
        }
        println!(
            "{:<10}{:>12.3}{:>12.3}{:>10.2}x{:>15.3} MiB",
            format!("{w} B"), mib(raw.len() as u64), mib(stored.len() as u64),
            raw.len() as f64 / s, mib((base - s) as u64)
        );
    }

    println!("\nthe column is {:.1}% of all part bytes today ({:.3} of {:.3} MiB)",
        hash_stored as f64 * 100.0 / part_bytes as f64, mib(hash_stored), mib(part_bytes));

    // Collision risk. This is the ONLY thing being traded away, so it gets stated exactly.
    println!("\n{:<12}{:>16}{:>16}{:>18}", "width", "1e6 pieces", "1e9 pieces", "1e12 pieces");
    for w in [32usize, 24, 16, 12, 8] {
        let bits = (w * 8) as f64;
        let f = |n: f64| {
            let l = collision_log2(n, bits);
            if l > -1.0 { "~certain".to_string() } else { format!("2^{:.0}", l) }
        };
        println!("{:<12}{:>16}{:>16}{:>18}", format!("{w} B"), f(1e6), f(1e9), f(1e12));
    }
    println!(
        "\nAccidental collision is the easy column. The hard one is ADVERSARIAL: a birthday attack on\n\
         a w-byte digest costs about 2^(4w) work, so 16 B = 2^64 (reachable by a determined attacker)\n\
         and 32 B = 2^128 (not). In this store a forced collision would let injected content resolve\n\
         to a DIFFERENT piece's location — a silent wrong read. Whether that matters depends on\n\
         whether anything untrusted can write."
    );
    Ok(())
}
