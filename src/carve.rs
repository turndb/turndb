//! The carve: how a body becomes spans — an **opinion with escape hatches**, never a lock-in.
//!
//! Carving decides dedup quality: where piece boundaries land is where identical content can be
//! recognised. The engine's opinion, measured on three real corpora (`.scratch/carve-*`,
//! `.scratch/cdc/`), is **message-boundary structural carving**: full-resend traffic repeats
//! *exactly at message boundaries*, so cutting there found 10.9× duplication where CDC-4096 found
//! 3.7× and fixed blocks 2.6× — and under boundary shift (which every appended turn causes) the
//! structural carve held 326× total collapse while everything content-defined degraded.
//! CDC is the right tool when structure is unknown; here it is known, so CDC is the FALLBACK.
//!
//! # The escape hatches, in order of reach
//!
//! * pick a different [`Carve`] variant per call ([`crate::store::Store::put_body_with`]);
//! * hand the store your own spans ([`crate::store::Store::put`]) — the carve is bypassed
//!   entirely, which is why nothing below this module ever depends on how spans were made;
//! * change the default later: the WAL stores *carved results*, so replay never re-runs a carve,
//!   and a changed opinion affects only new writes (and costs only dedup against old boundaries
//!   until a re-fold re-carves history).
//!
//! # Determinism
//!
//! Everything here is deterministic — the CDC gear table derives from a fixed seed — because two
//! writers carving the same bytes must find the same pieces or dedup silently halves. Changing
//! the table (or any boundary rule) is SAFE for correctness and expensive for dedup; it is a
//! dial, not a version lever.

use crate::store::Span;
use std::ops::Range;
use std::sync::OnceLock;

/// The carving strategy. `Default` is the engine's opinion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carve {
    /// Message-boundary structural carving: the elements of a top-level JSON array become pieces,
    /// the punctuation between them stays inline. A body that is not a JSON array falls back to
    /// [`Carve::Cdc`] at 4096. `intra`: additionally chunk INSIDE any single element larger than
    /// this many bytes, so a small edit to a huge tool result does not forfeit the whole piece.
    Messages { intra: Option<usize> },
    /// Content-defined chunking at a target size (FastCDC-style normalisation; chunks land in
    /// `[target/4, target*4]`). The structure-blind fallback, and the escape hatch for bodies
    /// with structure this module does not know.
    Cdc { target: usize },
    /// The whole body as one piece: dedup at whole-body granularity only.
    Whole,
    /// Nothing folded — the body stays inline in the record. The opt-out, and the baseline
    /// against which every other strategy is measured.
    Inline,
}

impl Default for Carve {
    fn default() -> Carve {
        Carve::Messages { intra: None }
    }
}

impl Carve {
    /// Carve `body` into spans. Concatenating the spans reproduces `body` byte for byte — the
    /// property every strategy must hold and the tests enforce; everything else is economics.
    pub fn carve<'a>(&self, body: &'a [u8]) -> Vec<Span<'a>> {
        let ranges = self.ranges(body);
        ranges
            .into_iter()
            .map(|(foldable, r)| if foldable { Span::Piece(&body[r]) } else { Span::Lit(&body[r]) })
            .collect()
    }

    /// The carve as `(foldable, range)` pairs — the measurable form the strategies produce.
    pub fn ranges(&self, body: &[u8]) -> Vec<(bool, Range<usize>)> {
        if body.is_empty() {
            return Vec::new();
        }
        match *self {
            Carve::Inline => vec![(false, 0..body.len())],
            Carve::Whole => vec![(true, 0..body.len())],
            Carve::Cdc { target } => gear_ranges(body, target.max(64)),
            Carve::Messages { intra } => match split_json_array(body) {
                Some(elems) if !elems.is_empty() => {
                    let mut out = Vec::with_capacity(elems.len() * 2 + 1);
                    let mut cur = 0usize;
                    for (a, b) in elems {
                        if a > cur {
                            out.push((false, cur..a));
                        }
                        match intra {
                            Some(big) if b - a > big => {
                                for (_, sub) in gear_ranges(&body[a..b], big.max(64)) {
                                    out.push((true, a + sub.start..a + sub.end));
                                }
                            }
                            _ => out.push((true, a..b)),
                        }
                        cur = b;
                    }
                    if cur < body.len() {
                        out.push((false, cur..body.len()));
                    }
                    out
                }
                // Not a message array: structure unknown, so the structure-blind tool takes over.
                _ => gear_ranges(body, 4096),
            },
        }
    }
}

/// Byte ranges of the top-level elements of a JSON array, string- and escape-aware. `None` when
/// `body` is not a JSON array — malformed JSON inside a well-formed array is the ELEMENTS'
/// problem and splits fine; a missing closing bracket is not, and refuses to `None` rather than
/// guess at boundaries that a resend would then fail to reproduce.
fn split_json_array(s: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_whitespace() {
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
                w if w.is_ascii_whitespace() => {}
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

/// splitmix64 — the gear table's deterministic generator.
fn sm64(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

/// The gear table, from a FIXED seed: every build of this engine cuts identical chunks. See the
/// module note on determinism — this is a dedup dial, not a version lever.
fn gear_table() -> &'static [u64; 256] {
    static TBL: OnceLock<[u64; 256]> = OnceLock::new();
    TBL.get_or_init(|| {
        let mut s = 0x7475726e_64620001u64; // "turndb", 1
        let mut t = [0u64; 256];
        for e in t.iter_mut() {
            *e = sm64(&mut s);
        }
        t
    })
}

/// Gear CDC with FastCDC-style normalisation: a stricter mask before the target size, a looser
/// one after, so chunk sizes concentrate around `avg`. Chunks land in `[avg/4, avg*4]`, except a
/// short final remainder.
fn gear_ranges(body: &[u8], avg: usize) -> Vec<(bool, Range<usize>)> {
    let tbl = gear_table();
    let min = avg / 4;
    let max = avg * 4;
    let bits = (avg as f64).log2().round() as u32;
    let ns = (bits + 2).min(63); // strict: harder to cut
    let nl = bits.saturating_sub(2).max(1); // loose: easier to cut
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < body.len() {
        let rem = body.len() - start;
        if rem <= min {
            out.push((true, start..body.len()));
            break;
        }
        let hard = (start + max).min(body.len());
        let mut h: u64 = 0;
        let mut i = start + min;
        // prime the hash over the skipped minimum so the cut still depends on those bytes
        for &b in &body[start..start + min] {
            h = (h << 1).wrapping_add(tbl[b as usize]);
        }
        let normal = (start + avg).min(hard);
        let mut cut = hard;
        while i < hard {
            h = (h << 1).wrapping_add(tbl[body[i] as usize]);
            let n = if i < normal { ns } else { nl };
            if h >> (64 - n) == 0 {
                cut = i + 1;
                break;
            }
            i += 1;
        }
        out.push((true, start..cut));
        start = cut;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reassemble(body: &[u8], ranges: &[(bool, Range<usize>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (_, r) in ranges {
            out.extend_from_slice(&body[r.clone()]);
        }
        out
    }

    /// Every strategy's one obligation: the spans reproduce the body byte for byte.
    #[test]
    fn every_strategy_reassembles_exactly() {
        let bodies: Vec<Vec<u8>> = vec![
            br#"[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}]"#.to_vec(),
            br#"[{"a":"br,ack]ets \" in [strings"},{"b":[1,2,{"c":"}]"}]},  "bare string"  ]"#
                .to_vec(),
            br#"{"not":"an array"}"#.to_vec(),
            (0u8..=255).cycle().take(40_000).collect(),
            b"tiny".to_vec(),
            Vec::new(),
        ];
        let strategies = [
            Carve::Messages { intra: None },
            Carve::Messages { intra: Some(1024) },
            Carve::Cdc { target: 2048 },
            Carve::Whole,
            Carve::Inline,
        ];
        for body in &bodies {
            for s in &strategies {
                let ranges = s.ranges(body);
                assert_eq!(&reassemble(body, &ranges), body, "{s:?} broke byte-exactness");
                // ranges must also be contiguous from 0 to len — no gaps, no overlaps
                let mut at = 0usize;
                for (_, r) in &ranges {
                    assert_eq!(r.start, at, "{s:?} left a gap");
                    at = r.end;
                }
                assert_eq!(at, body.len(), "{s:?} fell short");
            }
        }
    }

    /// THE property the default opinion exists for: appending a turn leaves every earlier
    /// message's piece boundaries — and therefore identities — untouched.
    #[test]
    fn appending_a_turn_preserves_every_earlier_piece() {
        let turn = |i: usize| {
            format!(
                r#"{{"role":"user","content":"turn {i} with some padding {}"}}"#,
                "x".repeat(i * 13)
            )
        };
        let conv =
            |n: usize| format!("[{}]", (0..n).map(turn).collect::<Vec<_>>().join(",")).into_bytes();
        let carve = Carve::default();
        let pieces = |b: &[u8]| -> Vec<Vec<u8>> {
            carve.ranges(b).into_iter().filter(|(f, _)| *f).map(|(_, r)| b[r].to_vec()).collect()
        };
        let five = conv(5);
        let six = conv(6);
        let p5 = pieces(&five);
        let p6 = pieces(&six);
        assert_eq!(p5.len(), 5);
        assert_eq!(p6.len(), 6);
        assert_eq!(&p6[..5], &p5[..], "the resent prefix must carve to IDENTICAL pieces");
    }

    #[test]
    fn non_array_bodies_fall_back_to_cdc() {
        let body: Vec<u8> = (0..30_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let msg = Carve::Messages { intra: None }.ranges(&body);
        let cdc = Carve::Cdc { target: 4096 }.ranges(&body);
        assert_eq!(msg, cdc, "unknown structure must carve exactly as the fallback does");
        assert!(msg.len() > 1, "a large opaque body must actually chunk");
    }

    #[test]
    fn cdc_chunks_respect_their_bounds_and_determinism() {
        let body: Vec<u8> = {
            let mut s = 42u64;
            (0..200_000).map(|_| (sm64(&mut s) & 0xFF) as u8).collect()
        };
        let avg = 4096;
        let ranges = Carve::Cdc { target: avg }.ranges(&body);
        for (i, (_, r)) in ranges.iter().enumerate() {
            let last = i + 1 == ranges.len();
            assert!(r.len() <= avg * 4, "chunk {i} exceeds max");
            if !last {
                assert!(r.len() >= avg / 4, "chunk {i} under min");
            }
        }
        assert_eq!(ranges, Carve::Cdc { target: avg }.ranges(&body), "CDC must be deterministic");
    }

    /// A shifted copy re-synchronises: content-defined cuts recover identical chunks after a
    /// prefix insertion, once past a resync window. (This is what CDC buys over fixed blocks,
    /// and what the fallback is for.)
    #[test]
    fn cdc_resynchronises_after_a_prefix_shift() {
        let body: Vec<u8> = {
            let mut s = 7u64;
            (0..300_000).map(|_| (sm64(&mut s) & 0xFF) as u8).collect()
        };
        let mut shifted = b"PREFIX-INSERTED-".to_vec();
        shifted.extend_from_slice(&body);
        let a: std::collections::HashSet<Vec<u8>> = Carve::Cdc { target: 4096 }
            .ranges(&body)
            .into_iter()
            .map(|(_, r)| body[r].to_vec())
            .collect();
        let b: std::collections::HashSet<Vec<u8>> = Carve::Cdc { target: 4096 }
            .ranges(&shifted)
            .into_iter()
            .map(|(_, r)| shifted[r].to_vec())
            .collect();
        let common = a.intersection(&b).count();
        assert!(
            common * 10 >= a.len() * 8,
            "at least 80% of chunks must survive a prefix shift; got {common}/{}",
            a.len()
        );
    }

    #[test]
    fn intra_chunking_splits_only_oversized_elements() {
        let small = r#"{"role":"user","content":"short"}"#;
        let big = format!(r#"{{"role":"tool","content":"{}"}}"#, "y".repeat(20_000));
        let body = format!("[{small},{big}]").into_bytes();
        let plain = Carve::Messages { intra: None }.ranges(&body);
        let hybrid = Carve::Messages { intra: Some(4096) }.ranges(&body);
        let fold_count = |v: &[(bool, Range<usize>)]| v.iter().filter(|(f, _)| *f).count();
        assert_eq!(fold_count(&plain), 2);
        assert!(fold_count(&hybrid) > 2, "the oversized element must chunk");
        // and the small element's piece is identical in both
        let first_piece = |v: &[(bool, Range<usize>)]| {
            v.iter().find(|(f, _)| *f).map(|(_, r)| r.clone()).unwrap()
        };
        assert_eq!(first_piece(&plain), first_piece(&hybrid));
    }
}
