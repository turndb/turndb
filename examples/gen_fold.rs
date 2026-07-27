//! Generate an incompressible multi-segment fold — the open-time measurement fixture.
//!
//! usage: gen_fold <dir> <total-mib> <seg-max-mib>

use std::path::PathBuf;
use turndb::fold::{Fold, FoldCfg};

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let dir = PathBuf::from(a.next().expect("usage: gen_fold <dir> <total-mib> <seg-max-mib>"));
    let total: u64 = a.next().and_then(|s| s.parse().ok()).unwrap_or(256);
    let seg: u32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(32);
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = FoldCfg { seg_max: seg * 1024 * 1024, block_target: 4 << 20, level: 1, ..Default::default() };
    let mut f = Fold::open(&dir, cfg)?;
    let mut h = blake3::hash(b"seed");
    let mut piece = Vec::with_capacity(64 * 1024);
    let mut written = 0u64;
    while written < total * 1024 * 1024 {
        piece.clear();
        while piece.len() < 64 * 1024 {
            piece.extend_from_slice(h.as_bytes());
            h = blake3::hash(h.as_bytes());
        }
        f.put(&piece)?;
        written += piece.len() as u64;
        if f.window_len() > 4096 {
            f.seal_window();
        }
    }
    f.sync()?;
    println!("{} segments, {} bytes", f.segment_count(), f.disk_bytes());
    Ok(())
}
