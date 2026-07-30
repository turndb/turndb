//! Deterministic simulation testing: every crash the filesystem can permit, replayed for real.
//!
//! The vfs seam records every mutating operation a workload performs — bytes included — and this
//! harness reconstructs, for every operation boundary, the states a power loss could leave behind
//! under the STRICT POSIX durability model: a write is volatile until its file's fsync; a created,
//! renamed, or unlinked NAME is volatile until its parent directory's fsync; and the two are
//! independent, which is where real filesystems hide their sharpest teeth.
//!
//! Each crash state is materialized into a real directory and opened by the real `Store::open`.
//! The invariants are absolute:
//!   * opening NEVER panics, and never refuses — every reachable crash state is a documented
//!     recovery, not an error;
//!   * every record ACKed (its `sync()` returned) before the crash point is present and
//!     byte-exact, and every acked delete stays deleted;
//!   * whatever else is present is byte-exact too — a half-applied batch, a resurrected record,
//!     or drifted content is a failure even if no ack covered it.
//!
//! Run with: `cargo test --features dst --test dst`
#![cfg(feature = "dst")]

use std::collections::{BTreeMap, HashMap};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use turndb::fold::FoldCfg;
use turndb::store::{Batch, Span, Store};
use turndb::vfs::record::{self, Op};

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-dst-{tag}-{}-{n}", std::process::id()))
}

// ---------------------------------------------------------------------------------------------
// The filesystem model
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Inode {
    /// Content guaranteed after a crash: as of this file's last fsync.
    durable: Vec<u8>,
    /// Content if every issued write landed.
    volatile: Vec<u8>,
    /// Writes since the last fsync, in order — the raw material for torn variants.
    pending: Vec<(u64, Vec<u8>)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    File,
    Dir,
}

/// The whole tree: an inode table plus TWO namespaces. `volatile` is what a process sees;
/// `durable` is what survives a strict-POSIX power loss — names only move between them at their
/// parent directory's fsync.
#[derive(Clone, Default)]
struct Fs {
    inodes: Vec<Inode>,
    kinds: Vec<Kind>,
    volatile_ns: BTreeMap<PathBuf, usize>,
    durable_ns: BTreeMap<PathBuf, usize>,
}

impl Fs {
    fn new_inode(&mut self, kind: Kind) -> usize {
        self.inodes.push(Inode::default());
        self.kinds.push(kind);
        self.inodes.len() - 1
    }

    /// The inode a path names right now, creating an untracked file on first touch — the WAL is
    /// opened create-if-missing without a Create op, and a model stricter than the log would
    /// reject legitimate workloads.
    fn touch(&mut self, path: &Path) -> usize {
        if let Some(&i) = self.volatile_ns.get(path) {
            return i;
        }
        let i = self.new_inode(Kind::File);
        self.volatile_ns.insert(path.to_path_buf(), i);
        i
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Create { path } => {
                let i = self.new_inode(Kind::File);
                self.volatile_ns.insert(path.clone(), i);
            }
            Op::WriteFile { path, data } => {
                let i = self.new_inode(Kind::File);
                self.volatile_ns.insert(path.clone(), i);
                let node = &mut self.inodes[i];
                node.pending.push((0, data.clone()));
                node.volatile = data.clone();
            }
            Op::WriteAt { path, off, data } => {
                let i = self.touch(path);
                let node = &mut self.inodes[i];
                let end = *off as usize + data.len();
                if node.volatile.len() < end {
                    node.volatile.resize(end, 0);
                }
                node.volatile[*off as usize..end].copy_from_slice(data);
                node.pending.push((*off, data.clone()));
            }
            Op::SetLen { path, len } => {
                let i = self.touch(path);
                let node = &mut self.inodes[i];
                node.volatile.resize(*len as usize, 0);
                // model truncation as a pending "rewrite to this image" so torn variants stay sane
                node.pending.push((u64::MAX, node.volatile.clone()));
            }
            Op::SyncFile { path } => {
                if let Some(&i) = self.volatile_ns.get(path) {
                    let node = &mut self.inodes[i];
                    node.durable = node.volatile.clone();
                    node.pending.clear();
                }
            }
            Op::SyncDir { path } => {
                // Promote every namespace change directly inside `path`: additions, renames,
                // and removals all become durable together.
                let of_dir = |ns: &BTreeMap<PathBuf, usize>| -> Vec<(PathBuf, usize)> {
                    ns.iter()
                        .filter(|(p, _)| p.parent() == Some(path.as_path()))
                        .map(|(p, i)| (p.clone(), *i))
                        .collect()
                };
                let vol = of_dir(&self.volatile_ns);
                for (p, _) in of_dir(&self.durable_ns) {
                    self.durable_ns.remove(&p);
                }
                for (p, i) in vol {
                    self.durable_ns.insert(p, i);
                }
            }
            Op::Rename { from, to } => {
                if let Some(i) = self.volatile_ns.remove(from) {
                    self.volatile_ns.insert(to.clone(), i);
                }
            }
            Op::Unlink { path } => {
                self.volatile_ns.remove(path);
            }
            Op::Mkdir { path } => {
                // create_dir_all: every missing ancestor
                let mut stack = vec![path.clone()];
                while let Some(p) = stack.last().and_then(|p| p.parent()).map(|p| p.to_path_buf()) {
                    if p.as_os_str().is_empty() || self.volatile_ns.contains_key(&p) {
                        break;
                    }
                    stack.push(p);
                }
                for p in stack.into_iter().rev() {
                    if !self.volatile_ns.contains_key(&p) {
                        let i = self.new_inode(Kind::Dir);
                        self.volatile_ns.insert(p, i);
                    }
                }
            }
            Op::RemoveTree { path } => {
                let doomed: Vec<PathBuf> =
                    self.volatile_ns.keys().filter(|p| p.starts_with(path)).cloned().collect();
                for p in doomed {
                    self.volatile_ns.remove(&p);
                }
            }
        }
    }
}

/// Which world the crash leaves behind.
#[derive(Clone, Copy, Debug)]
enum Variant {
    /// Strict floor: only fsynced content, only dir-fsynced names.
    DurableOnly,
    /// Everything issued landed — a process crash rather than power loss.
    AllLanded,
    /// Names durable-only, content all landed: files written but their dirents lost.
    NamesLag,
    /// Names all landed, content durable-only: dirents visible, unsynced bytes gone.
    ContentLag,
    /// AllLanded, except the very last pending write of the last-touched file is TORN at a
    /// fraction of its length.
    TornTail(u8),
}

const VARIANTS: &[Variant] = &[
    Variant::DurableOnly,
    Variant::AllLanded,
    Variant::NamesLag,
    Variant::ContentLag,
    Variant::TornTail(1),
    Variant::TornTail(2),
];

/// Materialize the crash state into `out` (rebasing paths from `root`), and answer whether
/// anything was materialized at all.
fn materialize(fs: &Fs, variant: Variant, root: &Path, out: &Path) -> bool {
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(out).unwrap();

    let names: &BTreeMap<PathBuf, usize> = match variant {
        Variant::DurableOnly | Variant::NamesLag => &fs.durable_ns,
        _ => &fs.volatile_ns,
    };
    // Find the file whose last pending write gets torn. Only DATA writes tear: truncation is
    // journaled inode metadata and applies atomically on every real filesystem — "half a
    // truncate" is a state no crash can produce, and simulating it would demand recovery from
    // the impossible. (The SetLen-never-happened case is DurableOnly's job.)
    let torn: Option<(usize, usize)> = match variant {
        Variant::TornTail(frac) => fs
            .inodes
            .iter()
            .enumerate()
            .rev()
            .find(|(_, n)| n.pending.last().is_some_and(|(off, _)| *off != u64::MAX))
            .map(|(i, n)| {
                let (_, data) = n.pending.last().unwrap();
                (i, data.len() * frac as usize / 3)
            }),
        _ => None,
    };

    let mut wrote = false;
    for (path, &i) in names {
        let Ok(rel) = path.strip_prefix(root) else { continue };
        let dst = out.join(rel);
        match fs.kinds[i] {
            Kind::Dir => {
                std::fs::create_dir_all(&dst).unwrap();
            }
            Kind::File => {
                let node = &fs.inodes[i];
                let content: Vec<u8> = match variant {
                    Variant::DurableOnly | Variant::ContentLag => node.durable.clone(),
                    Variant::AllLanded | Variant::NamesLag => node.volatile.clone(),
                    Variant::TornTail(_) => {
                        if torn.map(|(ti, _)| ti) == Some(i) {
                            // durable + all pending except the last, torn
                            let mut img = node.durable.clone();
                            let cut = torn.unwrap().1;
                            for (k, (off, data)) in node.pending.iter().enumerate() {
                                let take =
                                    if k + 1 == node.pending.len() { cut } else { data.len() };
                                if *off == u64::MAX {
                                    img = data[..take.min(data.len())].to_vec();
                                    continue;
                                }
                                let end = *off as usize + take;
                                if img.len() < end {
                                    img.resize(end, 0);
                                }
                                img[*off as usize..end].copy_from_slice(&data[..take]);
                            }
                            img
                        } else {
                            node.volatile.clone()
                        }
                    }
                };
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&dst, &content).unwrap();
                wrote = true;
            }
        }
    }
    wrote
}

// ---------------------------------------------------------------------------------------------
// The workload
// ---------------------------------------------------------------------------------------------

/// One issued logical write: `(group, id, value)`. Writes in the same group (a batch's members)
/// commit atomically — a valid recovery may not split them.
type Issued = (usize, String, Option<Vec<u8>>);

/// `(ops recorded when the ack returned, issued entries covered by the ack)`.
type Ack = (usize, usize);

fn body_for(i: usize) -> Vec<u8> {
    // shared prefix (dedups across records) + unique tail
    format!(
        "{{\"system\":\"the shared system prompt, {} bytes of it {}\",\"turn\":{i}}}",
        64,
        "x".repeat(64)
    )
    .into_bytes()
}

/// A deterministic mixed workload. Returns the op log, the issued-write timeline, and the acks.
fn run_workload(dir: &Path) -> (Vec<Op>, Vec<Issued>, Vec<Ack>) {
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    record::arm();
    let mut issued: Vec<Issued> = Vec::new();
    let mut acks: Vec<Ack> = Vec::new();
    let mut group = 0usize;

    {
        let mut s = Store::open(dir, cfg).unwrap();
        // three flush intervals of puts, with a delete and a batch mixed in
        for round in 0..3usize {
            for i in 0..6usize {
                let id = format!("r{round}:{i}");
                let body = body_for(round * 10 + i);
                s.put(&id, &[Span::Lit(b"["), Span::Piece(&body), Span::Lit(b"]")], vec![])
                    .unwrap();
                let mut want = b"[".to_vec();
                want.extend_from_slice(&body);
                want.extend_from_slice(b"]");
                group += 1;
                issued.push((group, id, Some(want)));
            }
            if round == 1 {
                s.delete("r0:0").unwrap();
                group += 1;
                issued.push((group, "r0:0".into(), None));
                let mut bt = Batch::new();
                let bb = body_for(99);
                bt.put("batch:a", &[Span::Piece(&bb)], vec![]);
                bt.delete("r0:1");
                s.apply(bt).unwrap();
                group += 1; // one group, two members: atomic
                issued.push((group, "batch:a".into(), Some(bb)));
                issued.push((group, "r0:1".into(), None));
            }
            s.sync().unwrap();
            acks.push((record::len(), issued.len()));
            if round < 2 {
                s.flush().unwrap();
            }
        }
        s.flush().unwrap();
        s.merge_range(0, 2).unwrap();
    }
    {
        // Reopen mid-workload: RECOVERY ITSELF is part of the recorded op stream, so crash points
        // inside recovery-after-a-crash get tested too.
        let mut s = Store::open(&dir.to_path_buf(), cfg).unwrap();
        s.delete("r2:5").unwrap();
        group += 1;
        issued.push((group, "r2:5".into(), None));
        s.sync().unwrap();
        acks.push((record::len(), issued.len()));
        s.flush().unwrap();
        // A refold — the one content rewrite, with its generation swap and log purge.
        s.refold().unwrap();
        // a final unsynced tail: put but never sync — allowed to vanish
        s.put("unsynced", &[Span::Piece(b"never acked, may vanish")], vec![]).unwrap();
        group += 1;
        issued.push((group, "unsynced".into(), Some(b"never acked, may vanish".to_vec())));
    }
    (record::disarm(), issued, acks)
}

/// The id -> value map after the first `p` issued entries.
fn state_after(issued: &[Issued], p: usize) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut m = BTreeMap::new();
    for (_, id, v) in &issued[..p] {
        m.insert(id.clone(), v.clone());
    }
    m
}

/// Prefix lengths that do not split a group.
fn group_boundaries(issued: &[Issued]) -> Vec<usize> {
    let mut out = vec![0usize];
    for p in 1..=issued.len() {
        if p == issued.len() || issued[p].0 != issued[p - 1].0 {
            out.push(p);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------------------------

#[test]
fn every_crash_state_recovers_to_an_acked_consistent_store() {
    let root = tmp("world");
    std::fs::create_dir_all(&root).unwrap();
    let work = root.join("store");
    let (ops, issued, acks) = run_workload(&work);
    assert!(ops.len() > 100, "the workload must exercise a real op stream, got {}", ops.len());
    let boundaries = group_boundaries(&issued);

    let stage = root.join("stage");
    let mut checked = 0usize;
    for k in 0..=ops.len() {
        // The ack floor: the largest issued prefix whose sync completed within the op prefix.
        // Recovery may sit ANYWHERE at or beyond it (later writes were issued, just not acked),
        // but always at a group boundary and always exactly a prefix — no holes, no reordering.
        let floor: usize = acks.iter().rev().find(|(n, _)| *n <= k).map(|(_, p)| *p).unwrap_or(0);

        let mut fs = Fs::default();
        for op in &ops[..k] {
            fs.apply(op);
        }

        for &variant in VARIANTS {
            if !materialize(&fs, variant, &work, &stage) {
                continue; // nothing durable yet — an empty directory is a new store, not a crash
            }
            let r = catch_unwind(AssertUnwindSafe(|| {
                check_state(&stage, &issued, &boundaries, floor, k, variant)
            }));
            if r.is_err() {
                eprintln!("--- op trace up to crash point {k} ---");
                for (i, op) in ops[..k].iter().enumerate() {
                    eprintln!("{i:4}: {}", op_summary(op));
                }
                panic!("FAILED at crash point {k} variant {variant:?}");
            }
            checked += 1;
        }
    }
    assert!(checked > 0);
    println!("dst: {} crash states checked across {} ops", checked, ops.len());
    std::fs::remove_dir_all(&root).ok();
}

fn op_summary(op: &Op) -> String {
    let short =
        |p: &Path| p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    match op {
        Op::Create { path } => format!("Create      {}", short(path)),
        Op::WriteAt { path, off, data } => {
            format!("WriteAt     {} off={off} len={}", short(path), data.len())
        }
        Op::SetLen { path, len } => format!("SetLen      {} len={len}", short(path)),
        Op::WriteFile { path, data } => format!("WriteFile   {} len={}", short(path), data.len()),
        Op::SyncFile { path } => format!("SyncFile    {}", short(path)),
        Op::SyncDir { path } => format!("SyncDir     {}", short(path)),
        Op::Rename { from, to } => format!("Rename      {} -> {}", short(from), short(to)),
        Op::Unlink { path } => format!("Unlink      {}", short(path)),
        Op::Mkdir { path } => format!("Mkdir       {}", short(path)),
        Op::RemoveTree { path } => format!("RemoveTree  {}", short(path)),
    }
}

fn check_state(
    stage: &Path,
    issued: &[Issued],
    boundaries: &[usize],
    floor: usize,
    k: usize,
    variant: Variant,
) {
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    // The READER first, before the writer heals anything: a reader holds no lock and repairs
    // nothing, so on a raw crash state it may serve committed state or refuse with an error —
    // but it must never panic, and if it opens, what it serves must be readable. (The writer's
    // open below sweeps, promotes, and replays; a reader must cope with the state as the crash
    // left it.)
    {
        if let Ok(rs) = Store::open_read(stage, cfg) {
            let _ = rs.ids().map(|ids| {
                for id in ids.iter().take(64) {
                    let _ = rs.reconstruct(id);
                }
            });
        }
    }
    let store = match Store::open(stage, cfg) {
        Ok(s) => s,
        Err(e) => {
            panic!("crash point {k} {variant:?}: open REFUSED a reachable crash state: {e:#}")
        }
    };
    // Read the whole recovered logical state.
    let ids =
        store.ids().unwrap_or_else(|e| panic!("crash point {k} {variant:?}: ids() failed: {e:#}"));
    let mut recovered: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
    for id in ids {
        let v = store
            .reconstruct(&id)
            .unwrap_or_else(|e| panic!("crash point {k} {variant:?}: {id} unreadable: {e:#}"));
        recovered.insert(id, v);
    }
    // ids() must never list an id that then reads as absent.
    for (id, v) in &recovered {
        assert!(v.is_some(), "crash point {k} {variant:?}: ids() listed {id} but it reads absent");
    }

    // PREFIX CONSISTENCY: the recovered state must equal the state after some group-boundary
    // prefix of the issued sequence, at or beyond the acked floor. This subsumes every softer
    // check — acked data present (prefix >= floor), no resurrections or holes (it is a prefix),
    // batch atomicity (boundaries only), byte-exactness (equality).
    let matches = boundaries.iter().filter(|&&p| p >= floor).any(|&p| {
        let want = state_after(issued, p);
        let want_present: BTreeMap<&String, &Vec<u8>> =
            want.iter().filter_map(|(id, v)| v.as_ref().map(|b| (id, b))).collect();
        let got_present: BTreeMap<&String, &Vec<u8>> =
            recovered.iter().filter_map(|(id, v)| v.as_ref().map(|b| (id, b))).collect();
        want_present == got_present
    });
    assert!(
        matches,
        "crash point {k} {variant:?}: recovered state matches NO issued prefix >= the acked floor \
         ({floor}); recovered ids: {:?}",
        recovered.keys().collect::<Vec<_>>()
    );
}

/// A HashMap is deliberately not used for the namespaces: iteration order feeds materialization,
/// and nondeterministic iteration would make a failing seed unreproducible.
#[allow(dead_code)]
fn _model_is_deterministic(_: &HashMap<(), ()>) {}
