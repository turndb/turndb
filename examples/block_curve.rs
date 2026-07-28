//! The ratio/latency curve for block framing, measured on the workload that actually happens.
//!
//! usage: block_curve <corpus.jsonl>
//!
//! `block_experiment` showed blocking is a large ratio win and priced a single random piece read. That
//! is the worst case: cold cache, whole block decompressed, one piece used, everything thrown away.
//! The real read is **reconstruct a record** — dozens of pieces that were captured together and so
//! cluster in the fold — and a decompressed-block cache serves the rest of them for free.
//!
//! Reports, per block size: compressed size, single-piece cost, and whole-record cost with a small
//! block cache. That is the curve the format decision should be made on.

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

/// An LRU of decompressed blocks, bounded by BYTES — as the fold's own cache is.
///
/// This was bounded by block COUNT, which silently rigged the whole comparison: 8 blocks is 512 KiB
/// of cache at 64 KiB blocks and 128 MiB at 16 MiB blocks, so the largest sizes were being handed 256x
/// more memory than the smallest and then credited with the resulting hit rate. A byte budget is what
/// a reader actually has, and it is the only way the read column means anything across the sweep.
struct BlockCache {
    budget: usize,
    bytes: usize,
    map: HashMap<usize, (u64, Vec<u8>)>,
    clock: u64,
    pub misses: u64,
}

impl BlockCache {
    fn new(budget: usize) -> Self {
        BlockCache { budget, bytes: 0, map: HashMap::new(), clock: 0, misses: 0 }
    }
    fn get<'a>(&'a mut self, bi: usize, comp: &[Vec<u8>], raw_len: usize) -> &'a [u8] {
        self.clock += 1;
        if !self.map.contains_key(&bi) {
            self.misses += 1;
            // admit one block however large, then evict coldest back inside the budget
            while self.bytes + raw_len > self.budget && !self.map.is_empty() {
                if let Some((&victim, _)) = self.map.iter().min_by_key(|(_, (t, _))| *t) {
                    if let Some((_, gone)) = self.map.remove(&victim) {
                        self.bytes -= gone.len();
                    }
                }
            }
            let d = zstd::bulk::decompress(&comp[bi], raw_len).unwrap();
            self.bytes += d.len();
            self.map.insert(bi, (self.clock, d));
        }
        let e = self.map.get_mut(&bi).unwrap();
        e.0 = self.clock;
        &e.1
    }
}

/// The reader's cache budget — the fold's own default, so the sweep is priced against reality.
const CACHE_BYTES: usize = 64 << 20;

fn main() -> anyhow::Result<()> {
    let corpus =
        PathBuf::from(std::env::args().nth(1).expect("usage: block_curve <corpus.jsonl> [field]"));
    let field = std::env::args().nth(2).unwrap_or_else(|| "body".to_string());

    // distinct pieces in capture order + each record's piece list
    let mut index: HashMap<[u8; 32], u32> = HashMap::new();
    let mut pieces: Vec<Vec<u8>> = Vec::new();
    let mut records: Vec<Vec<u32>> = Vec::new();
    let mut logical = 0u64;
    let rdr: Box<dyn BufRead> = if corpus.as_os_str() == "-" {
        Box::new(BufReader::with_capacity(1 << 22, std::io::stdin().lock()))
    } else {
        Box::new(BufReader::with_capacity(1 << 22, std::fs::File::open(&corpus)?))
    };
    for line in rdr.lines() {
        let line = line?;
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let body = match v.get(&field).and_then(|b| b.as_str()) {
            Some(b) => b.as_bytes().to_vec(),
            None => continue,
        };
        logical += body.len() as u64;
        let spans = match split_json_array(&body) {
            Some(e) if !e.is_empty() => e,
            _ => vec![(0, body.len())],
        };
        let mut refs = Vec::with_capacity(spans.len());
        for (a, b) in spans {
            let span = &body[a..b];
            let h: [u8; 32] = blake3::hash(span).into();
            let idx = *index.entry(h).or_insert_with(|| {
                pieces.push(span.to_vec());
                (pieces.len() - 1) as u32
            });
            refs.push(idx);
        }
        records.push(refs);
    }
    let raw: u64 = pieces.iter().map(|p| p.len() as u64).sum();
    let avg_refs = records.iter().map(|r| r.len()).sum::<usize>() as f64 / records.len() as f64;
    println!(
        "logical {:.2} MiB | {} records | {} distinct pieces ({:.2} MiB) | {:.0} refs/record",
        mib(logical),
        records.len(),
        pieces.len(),
        mib(raw),
        avg_refs
    );

    // sample of records to reconstruct, spread across the corpus
    let sample: Vec<usize> = (0..1000).map(|i| (i * 7919) % records.len()).collect();

    // ---- baseline: per-piece framing ----
    println!(
        "\n{:<16}{:>11}{:>10}{:>13}{:>15}{:>12}",
        "scheme", "size MiB", "overall", "1 piece us", "1 record us", "blk/record"
    );
    for lvl in [3, 19] {
        let comp: Vec<Vec<u8>> =
            pieces.iter().map(|p| zstd::bulk::compress(p, lvl).unwrap()).collect();
        let size: u64 = comp.iter().map(|c| c.len() as u64).sum::<u64>() + 16 * pieces.len() as u64;

        let t = Instant::now();
        for &i in &sample {
            std::hint::black_box(
                zstd::bulk::decompress(
                    &comp[i % pieces.len()],
                    pieces[i % pieces.len()].len().max(1),
                )
                .unwrap(),
            );
        }
        let one = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;

        let t = Instant::now();
        for &r in &sample {
            let mut out = Vec::new();
            for &pi in &records[r] {
                let pi = pi as usize;
                out.extend_from_slice(
                    &zstd::bulk::decompress(&comp[pi], pieces[pi].len().max(1)).unwrap(),
                );
            }
            std::hint::black_box(out);
        }
        let rec = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;
        println!(
            "{:<16}{:>11.2}{:>9.1}x{:>13.1}{:>15.1}{:>12}",
            format!("per-piece/z{lvl}"),
            mib(size),
            logical as f64 / size as f64,
            one,
            rec,
            "-"
        );
    }

    // ---- block framing across the curve ----
    // The sweep used to stop at 4 MiB, so 4 MiB was chosen as the largest size TESTED rather than as
    // a measured optimum. Extended, because the ratio was still climbing steeply there.
    for block_bytes in
        [64 * 1024usize, 1024 * 1024, 4 * 1024 * 1024, 16 * 1024 * 1024, 64 * 1024 * 1024]
    {
        // lay pieces into blocks in capture order
        let (mut blocks, mut where_of) =
            (Vec::<Vec<u8>>::new(), Vec::<(usize, usize, usize)>::new());
        let mut buf: Vec<u8> = Vec::new();
        for p in &pieces {
            let off = buf.len();
            buf.extend_from_slice(p);
            where_of.push((blocks.len(), off, p.len()));
            if buf.len() >= block_bytes {
                blocks.push(std::mem::take(&mut buf));
            }
        }
        if !buf.is_empty() {
            blocks.push(buf);
        }

        for lvl in [19] {
            let comp: Vec<Vec<u8>> =
                blocks.iter().map(|b| zstd::bulk::compress(b, lvl).unwrap()).collect();
            let index_bytes = pieces.len() as u64 * 10 + blocks.len() as u64 * 8;
            let size: u64 = comp.iter().map(|c| c.len() as u64).sum::<u64>() + index_bytes;

            // one random piece, cold cache — the worst case
            let t = Instant::now();
            for &i in &sample {
                let (bi, off, len) = where_of[i % pieces.len()];
                let whole = zstd::bulk::decompress(&comp[bi], blocks[bi].len()).unwrap();
                std::hint::black_box(&whole[off..off + len]);
            }
            let one = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;

            // a whole record, with a small block cache — the real read
            let mut cache = BlockCache::new(CACHE_BYTES);
            let t = Instant::now();
            for &r in &sample {
                let mut out = Vec::new();
                for &pi in &records[r] {
                    let (bi, off, len) = where_of[pi as usize];
                    let blk = cache.get(bi, &comp, blocks[bi].len());
                    out.extend_from_slice(&blk[off..off + len]);
                }
                std::hint::black_box(out);
            }
            let rec = t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64;
            let bpr = cache.misses as f64 / sample.len() as f64;
            println!(
                "{:<16}{:>11.2}{:>9.1}x{:>13.1}{:>15.1}{:>12.2}",
                format!("{}K/z{lvl}", block_bytes / 1024),
                mib(size),
                logical as f64 / size as f64,
                one,
                rec,
                bpr
            );
        }
    }
    Ok(())
}
