//! What does the piece dictionary's ORDER actually cost, and what could a different one buy?
//!
//! The dictionary's sort order is three decisions bundled into one, because a piece's ordinal *is* its
//! row index:
//!
//!   1. ordinal assignment  -> the varint width of every reference in `prog`
//!   2. physical row order  -> how well `pdict.loc` and `pdict.hash` compress
//!   3. search order        -> already unbundled, into `pdict.hsort`
//!
//! Fold order was chosen for (2). This measures what (1) is paying for that choice.
//!
//! usage: dict_order <store-dir>

use std::collections::HashMap;
use std::path::PathBuf;
use turndb::part::Part;
use turndb::BodyOp;

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: dict_order <dir>"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "part").unwrap_or(false))
        .collect();
    paths.sort();

    let (mut cur, mut byfreq, mut refs, mut distinct) = (0u64, 0u64, 0u64, 0u64);
    let mut hist = [0u64; 5];
    let mut top_share = Vec::new();

    for p in &paths {
        let part = Part::open(p)?;
        let n = part.piece_count()?;
        // hash -> current ordinal (which is fold order)
        let mut ord: HashMap<[u8; 32], usize> = HashMap::with_capacity(n);
        for i in 0..n {
            ord.insert(part.piece(i)?.1 .0, i);
        }
        // reference counts per ordinal
        let mut freq = vec![0u64; n];
        for r in 0..part.len() {
            for op in part.body(r)? {
                if let BodyOp::Piece { hash, .. } = op {
                    freq[ord[&hash.0]] += 1;
                }
            }
        }
        // current cost: ordinal is fold position, uncorrelated with how often it is used
        for (i, f) in freq.iter().enumerate() {
            cur += f * varint_len(((i as u64) << 1) | 1) as u64;
            refs += f;
        }
        // counterfactual: ordinals assigned by descending reference count
        let mut by: Vec<usize> = (0..n).collect();
        by.sort_by_key(|&i| std::cmp::Reverse(freq[i]));
        for (rank, &i) in by.iter().enumerate() {
            byfreq += freq[i] * varint_len(((rank as u64) << 1) | 1) as u64;
        }
        // skew: how concentrated are references?
        let total: u64 = freq.iter().sum();
        let hot: u64 = by.iter().take(n / 100).map(|&i| freq[i]).sum();
        top_share.push(hot as f64 * 100.0 / total.max(1) as f64);
        for &i in &by {
            let b = match freq[i] {
                0 => 0,
                1 => 1,
                2..=9 => 2,
                10..=99 => 3,
                _ => 4,
            };
            hist[b] += 1;
        }
        distinct += n as u64;
    }

    println!(
        "{} parts, {refs} piece refs over {distinct} distinct pieces ({:.1} refs/piece)\n",
        paths.len(),
        refs as f64 / distinct as f64
    );

    println!(
        "reference skew (per part, top 1% of pieces): {:.1}% of all refs",
        top_share.iter().sum::<f64>() / top_share.len() as f64
    );
    let names = ["unreferenced", "1 ref", "2-9 refs", "10-99 refs", "100+ refs"];
    for (i, n) in names.iter().enumerate() {
        println!("  {:<14}{:>10}  ({:.1}%)", n, hist[i], hist[i] as f64 * 100.0 / distinct as f64);
    }

    println!("\nprog piece-reference bytes (pre-compression):");
    println!("  fold order (today)      {:>9.3} MiB", mib(cur));
    println!(
        "  frequency order         {:>9.3} MiB  ({:.1}% smaller)",
        mib(byfreq),
        (cur - byfreq.min(cur)) as f64 * 100.0 / cur as f64
    );
    Ok(())
}
