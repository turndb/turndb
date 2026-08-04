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
//!     byte-exact — every named content, every attribute bit, NaN payloads included — and every
//!     acked delete stays deleted;
//!   * whatever else is present is byte-exact too — a half-applied batch, a resurrected record,
//!     or drifted content is a failure even if no ack covered it.
//!
//! Beyond the write path, each PUBLICATION PROTOCOL gets its own crash sweep: backup, restore,
//! manifest recovery promotion, hole punching, and format migration each run once for real, and
//! then every op prefix × durability variant is replayed against protocol-specific invariants.
//!
//! Run with: `cargo test --features dst --test dst`
#![cfg(feature = "dst")]

use std::collections::{BTreeMap, HashMap};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use turndb::fold::FoldCfg;
use turndb::store::{Batch, ContentSpans, RecoveryOptions, Span, Store};
use turndb::vfs::record::{self, Op};
use turndb::{AttrValue, BODY_CONTENT};

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

    /// Snapshot an on-disk tree as the fully DURABLE starting state: every file fsynced, every
    /// dirent promoted. Protocol sweeps start from a healthy committed store, so the recording
    /// does not have to reach back through the store's construction — the main workload test
    /// already covers those crash points.
    fn seed_durable(&mut self, dir: &Path) {
        let i = self.new_inode(Kind::Dir);
        self.volatile_ns.insert(dir.to_path_buf(), i);
        self.durable_ns.insert(dir.to_path_buf(), i);
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                self.seed_durable(&p);
            } else {
                let content = std::fs::read(&p).unwrap();
                let i = self.new_inode(Kind::File);
                self.inodes[i].durable = content.clone();
                self.inodes[i].volatile = content;
                self.volatile_ns.insert(p.clone(), i);
                self.durable_ns.insert(p, i);
            }
        }
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
            Op::PunchHole { path, off, len } => {
                // Deallocation reads back as zeros, so it is a data write of zeros for the model:
                // volatile until the file's fsync, and torn variants may land only part of it —
                // fallocate is per-extent, so a crash mid-punch genuinely can half-zero a range.
                let i = self.touch(path);
                let node = &mut self.inodes[i];
                let end = (*off + *len) as usize;
                if node.volatile.len() < end {
                    node.volatile.resize(end, 0);
                }
                node.volatile[*off as usize..end].fill(0);
                node.pending.push((*off, vec![0u8; *len as usize]));
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
                    if self.kinds[i] == Kind::Dir {
                        // Renaming a DIRECTORY: a real filesystem keys children inside the moved
                        // inode, so they follow it for free; this model keys them by path and must
                        // rebase them — in BOTH namespaces, because child dirents were made
                        // durable by the child directories' own fsyncs and are path-independent.
                        // What stays gated on the parent's fsync is the top-level name: a crash in
                        // which the rename never landed is the preceding op prefix (source tree
                        // intact, destination absent), and one in which it landed is this one.
                        let rebase = |ns: &mut BTreeMap<PathBuf, usize>| {
                            let moved: Vec<(PathBuf, usize)> = ns
                                .iter()
                                .filter(|(p, _)| p.starts_with(from))
                                .map(|(p, i)| (p.clone(), *i))
                                .collect();
                            for (p, _) in &moved {
                                ns.remove(p);
                            }
                            for (p, i) in moved {
                                let rel = p.strip_prefix(from).unwrap().to_path_buf();
                                ns.insert(to.join(rel), i);
                            }
                        };
                        rebase(&mut self.volatile_ns);
                        rebase(&mut self.durable_ns);
                    }
                    self.volatile_ns.insert(to.clone(), i);
                }
            }
            Op::Link { from, to } => {
                if let Some(&i) = self.volatile_ns.get(from) {
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

/// What one acked record must read back as: every named content byte-exact, every attribute
/// bit-exact and in order. `AttrValue` equality compares floats by bit pattern, which is what
/// makes the NaN-payload attr below an assertion instead of a tautology.
#[derive(Clone, PartialEq)]
struct Expect {
    contents: Vec<(String, Vec<u8>)>,
    attrs: Vec<(String, AttrValue)>,
}

impl Expect {
    fn body(bytes: Vec<u8>) -> Expect {
        Expect { contents: vec![(BODY_CONTENT.to_string(), bytes)], attrs: Vec::new() }
    }
}

/// One issued logical write: `(group, id, value)`. Writes in the same group (a batch's members)
/// commit atomically — a valid recovery may not split them.
type Issued = (usize, String, Option<Expect>);

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

/// The scalar-attr corners: the extremes the columnar attr encodings must round-trip exactly.
fn corner_attrs() -> Vec<(String, AttrValue)> {
    vec![
        ("max".into(), AttrValue::UInt(u64::MAX)),
        ("bin".into(), AttrValue::Bytes(vec![0x00, 0xFF, 0x00, 0x10, 0xFF])),
        ("born".into(), AttrValue::TimestampNs(i64::MIN)),
        ("gone".into(), AttrValue::Null),
        // A quiet NaN with a NONSTANDARD payload. Value equality calls every NaN the same;
        // bit equality is the store's contract, and this payload is what proves it held.
        ("nan".into(), AttrValue::Float(f64::from_bits(0x7FF8_DEAD_BEEF_F00D))),
    ]
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
                issued.push((group, id, Some(Expect::body(want))));
            }
            // One record per round with TWO named contents and the scalar-attr corners — the
            // part of the record model the single-content puts above never reach.
            {
                let id = format!("m{round}");
                let req = body_for(round * 10 + 7);
                let resp = body_for(round * 10 + 8);
                s.put_record(
                    &id,
                    &[
                        ContentSpans::new(
                            "req",
                            vec![Span::Lit(b"<"), Span::Piece(&req), Span::Lit(b">")],
                        ),
                        ContentSpans::new("resp", vec![Span::Piece(&resp)]),
                    ],
                    corner_attrs(),
                )
                .unwrap();
                let mut want_req = b"<".to_vec();
                want_req.extend_from_slice(&req);
                want_req.extend_from_slice(b">");
                group += 1;
                issued.push((
                    group,
                    id,
                    Some(Expect {
                        contents: vec![("req".into(), want_req), ("resp".into(), resp)],
                        attrs: corner_attrs(),
                    }),
                ));
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
                issued.push((group, "batch:a".into(), Some(Expect::body(bb))));
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
        let mut s = Store::open(dir, cfg).unwrap();
        s.delete("r2:5").unwrap();
        group += 1;
        issued.push((group, "r2:5".into(), None));
        s.sync().unwrap();
        acks.push((record::len(), issued.len()));
        s.flush().unwrap();
        // ERASURE — which is also the workload's one content rewrite. `erase_ids` composes the
        // tombstone batch, a total merge, and the re-fold with its generation swap, retained-log
        // purge, and log truncation: everything the standalone refold() here used to exercise,
        // now with content genuinely dropped and the erasure protocol wrapped around it.
        let erased = s.erase_ids(&["r1:2".into(), "never-existed".into()]).unwrap();
        assert_eq!(
            (erased.tombstoned, erased.absent),
            (1, 1),
            "the erase must hit exactly one live id and record the absent one"
        );
        group += 1;
        issued.push((group, "r1:2".into(), None));
        acks.push((record::len(), issued.len()));
        // a final unsynced tail: put but never sync — allowed to vanish
        s.put("unsynced", &[Span::Piece(b"never acked, may vanish")], vec![]).unwrap();
        group += 1;
        issued.push((
            group,
            "unsynced".into(),
            Some(Expect::body(b"never acked, may vanish".to_vec())),
        ));
    }
    (record::disarm(), issued, acks)
}

/// The id -> value map after the first `p` issued entries.
fn state_after(issued: &[Issued], p: usize) -> BTreeMap<String, Option<Expect>> {
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
        Op::PunchHole { path, off, len } => {
            format!("PunchHole   {} off={off} len={len}", short(path))
        }
        Op::SyncFile { path } => format!("SyncFile    {}", short(path)),
        Op::SyncDir { path } => format!("SyncDir     {}", short(path)),
        Op::Rename { from, to } => format!("Rename      {} -> {}", short(from), short(to)),
        Op::Link { from, to } => format!("Link        {} -> {}", short(from), short(to)),
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
                    if let Ok(Some(rec)) = rs.get(id) {
                        for c in &rec.contents {
                            let _ = rs.reconstruct_content(id, &c.name);
                        }
                    }
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
    // Read the whole recovered logical state: every named content of every record, plus attrs.
    let ids =
        store.ids().unwrap_or_else(|e| panic!("crash point {k} {variant:?}: ids() failed: {e:#}"));
    let mut recovered: BTreeMap<String, Expect> = BTreeMap::new();
    for id in ids {
        let rec = store
            .get(&id)
            .unwrap_or_else(|e| panic!("crash point {k} {variant:?}: {id} unreadable: {e:#}"));
        // ids() must never list an id that then reads as absent.
        let Some(rec) = rec else {
            panic!("crash point {k} {variant:?}: ids() listed {id} but it reads absent")
        };
        let mut contents = Vec::new();
        for c in &rec.contents {
            let bytes = store
                .reconstruct_content(&id, &c.name)
                .unwrap_or_else(|e| {
                    panic!(
                        "crash point {k} {variant:?}: {id} content {:?} unreadable: {e:#}",
                        c.name
                    )
                })
                .unwrap_or_else(|| {
                    panic!(
                        "crash point {k} {variant:?}: {id} lists content {:?} but it reads absent",
                        c.name
                    )
                });
            contents.push((c.name.clone(), bytes));
        }
        recovered.insert(id, Expect { contents, attrs: rec.attrs });
    }

    // PREFIX CONSISTENCY: the recovered state must equal the state after some group-boundary
    // prefix of the issued sequence, at or beyond the acked floor. This subsumes every softer
    // check — acked data present (prefix >= floor), no resurrections or holes (it is a prefix),
    // batch atomicity (boundaries only), byte-exactness (equality — now spanning every named
    // content and every attribute bit, NaN payload included).
    let matches = boundaries.iter().filter(|&&p| p >= floor).any(|&p| {
        let want = state_after(issued, p);
        let want_present: BTreeMap<&String, &Expect> =
            want.iter().filter_map(|(id, v)| v.as_ref().map(|e| (id, e))).collect();
        let got_present: BTreeMap<&String, &Expect> = recovered.iter().collect();
        want_present == got_present
    });
    assert!(
        matches,
        "crash point {k} {variant:?}: recovered state matches NO issued prefix >= the acked floor \
         ({floor}); recovered ids: {:?}",
        recovered.keys().collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------------------------
// Publication-protocol crash sweeps
// ---------------------------------------------------------------------------------------------

/// Replay every op prefix × variant of one recorded protocol run on top of a durable baseline,
/// materializing each crash state under `stage` and handing it to `check`. Returns the number of
/// states checked. Every prefix and every variant is checked — nothing is subsampled.
fn replay_recorded(
    tag: &str,
    base: &Fs,
    root: &Path,
    ops: &[Op],
    stage: &Path,
    mut check: impl FnMut(&Path, usize, Variant),
) -> usize {
    let mut checked = 0usize;
    for k in 0..=ops.len() {
        let mut fs = base.clone();
        for op in &ops[..k] {
            fs.apply(op);
        }
        for &variant in VARIANTS {
            materialize(&fs, variant, root, stage);
            let r = catch_unwind(AssertUnwindSafe(|| check(stage, k, variant)));
            if r.is_err() {
                eprintln!("--- {tag}: op trace up to crash point {k} ---");
                for (i, op) in ops[..k].iter().enumerate() {
                    eprintln!("{i:4}: {}", op_summary(op));
                }
                panic!("{tag}: FAILED at crash point {k} variant {variant:?}");
            }
            checked += 1;
        }
    }
    checked
}

/// A small settled store under `dir`, closed cleanly, plus the id -> body map it must serve.
fn build_settled_store(dir: &Path, cfg: FoldCfg, tag: usize) -> BTreeMap<String, Vec<u8>> {
    let mut want = BTreeMap::new();
    let mut s = Store::open(dir, cfg).unwrap();
    // Two commits, so the snapshot spans two parts and its manifest carries a chain link.
    for (lo, hi) in [(0usize, 5usize), (5, 8)] {
        for i in lo..hi {
            let id = format!("b:{i}");
            let body = body_for(tag + i);
            s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
            want.insert(id, body);
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    want
}

#[test]
fn every_backup_crash_leaves_the_source_intact_and_the_artifact_all_or_nothing() {
    let root = tmp("backup");
    std::fs::create_dir_all(&root).unwrap();
    let work = root.join("store");
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    let want = build_settled_store(&work, cfg, 200);

    let out = root.join("backup.turndb");
    let mut base = Fs::default();
    base.seed_durable(&root);
    record::arm();
    {
        // The real protocol: writer open (its recovery included), then the staged, verified,
        // hard-linked-no-replace publication.
        let mut s = Store::open(&work, cfg).unwrap();
        s.backup(&out).unwrap();
    }
    let ops = record::disarm();
    // Measured at 18 ops: a settled writer open is nearly silent (empty WAL, nothing to sweep),
    // so the stream is the pack protocol itself — staging writes, fsync, link, unlink, sync-dir.
    assert!(ops.len() > 12, "the backup must exercise a real op stream, got {}", ops.len());

    let stage = tmp("backup-stage");
    let checked = replay_recorded("backup", &base, &root, &ops, &stage, |stage, k, variant| {
        // The SOURCE reopens consistent no matter where the export died: backup is read-only
        // with respect to the store's logical state.
        let src = Store::open(&stage.join("store"), cfg).unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: source store refused to open: {e:#}")
        });
        for (id, body) in &want {
            assert_eq!(
                src.reconstruct(id).unwrap().as_deref(),
                Some(body.as_slice()),
                "crash point {k} {variant:?}: source record {id} drifted"
            );
        }
        assert_eq!(
            src.ids().unwrap().len(),
            want.len(),
            "crash point {k} {variant:?}: source store gained or lost records"
        );
        drop(src);
        // The ARTIFACT is all-or-nothing at its final name: absent, or a complete pack that
        // verifies and serves every record byte-exact. A torn file at the final name is a
        // failure even though no acknowledgement covered it. (Staging litter beside it is
        // allowed — the writer sweep collects it inside a store; here it is inert bytes.)
        let dst = stage.join("backup.turndb");
        if dst.exists() {
            let pack = turndb::pack::Pack::open(&dst).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: a torn pack sits at the FINAL name: {e:#}")
            });
            pack.verify().unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: published pack fails verification: {e:#}")
            });
            let rs = turndb::store::open_read_pack(&dst, cfg).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: published pack refuses a reader: {e:#}")
            });
            for (id, body) in &want {
                assert_eq!(
                    rs.reconstruct(id).unwrap().as_deref(),
                    Some(body.as_slice()),
                    "crash point {k} {variant:?}: pack record {id} drifted"
                );
            }
        }
    });
    println!("dst backup: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

#[test]
fn every_restore_crash_leaves_the_destination_all_or_nothing() {
    let root = tmp("restore");
    std::fs::create_dir_all(&root).unwrap();
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    // The source store lives OUTSIDE the modeled root: only the pack and the restore protocol
    // itself are under test here.
    let srcdir = tmp("restore-src");
    let want = build_settled_store(&srcdir, cfg, 400);
    let pack_path = root.join("origin.turndb");
    turndb::pack::write(&srcdir, &pack_path).unwrap();
    std::fs::remove_dir_all(&srcdir).ok();

    let dest = root.join("restored");
    let mut base = Fs::default();
    base.seed_durable(&root);
    record::arm();
    turndb::pack::restore(&pack_path, &dest).unwrap();
    let ops = record::disarm();
    assert!(ops.len() > 20, "the restore must exercise a real op stream, got {}", ops.len());

    let stage = tmp("restore-stage");
    let checked = replay_recorded("restore", &base, &root, &ops, &stage, |stage, k, variant| {
        let dst = stage.join("restored");
        if dst.exists() {
            // Published means COMPLETE: the final name only ever appears via the no-replace
            // rename of a fully extracted, fully fsynced, validated staging tree.
            let mut s = Store::open(&dst, cfg).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: a partial store sits at the FINAL name: {e:#}")
            });
            for (id, body) in &want {
                assert_eq!(
                    s.reconstruct(id).unwrap().as_deref(),
                    Some(body.as_slice()),
                    "crash point {k} {variant:?}: restored record {id} drifted"
                );
            }
            assert_eq!(s.ids().unwrap().len(), want.len());
            s.verify().unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: restored store fails verification: {e:#}")
            });
        } else {
            // No destination: the crash must be recoverable by simply RE-RUNNING the restore.
            // Staging-directory litter is allowed to remain and must not block the retry.
            turndb::pack::restore(&stage.join("origin.turndb"), &dst).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: restore cannot be re-run: {e:#}")
            });
            let s = Store::open_read(&dst, cfg).unwrap();
            for (id, body) in &want {
                assert_eq!(s.reconstruct(id).unwrap().as_deref(), Some(body.as_slice()));
            }
        }
    });
    println!("dst restore: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

#[test]
fn every_recovery_crash_converges_on_the_promoted_timeline() {
    let root = tmp("recover");
    std::fs::create_dir_all(&root).unwrap();
    let work = root.join("store");
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    // Four commits, tracking the exact logical state at each, so "the promoted prefix" is a
    // concrete map rather than a mood.
    let mut per_commit: Vec<BTreeMap<String, Vec<u8>>> = Vec::new();
    {
        let mut s = Store::open(&work, cfg).unwrap();
        let mut now: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for c in 1usize..=4 {
            for i in 0..3usize {
                let id = format!("c{c}:{i}");
                let body = body_for(300 + c * 10 + i);
                s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
                now.insert(id, body);
            }
            s.sync().unwrap();
            s.flush().unwrap();
            per_commit.push(now.clone());
        }
    }
    // Damage the live commit pointer AND the two newest retained copies — the shape checked
    // recovery exists for. The damage is the operator's starting point, not a protocol step, so
    // it is baseline state rather than recorded ops: what gets crash-swept is recovery itself.
    for name in ["MANIFEST", "MANIFEST.00000004", "MANIFEST.00000003"] {
        let p = work.join(name);
        let mut b = std::fs::read(&p).unwrap();
        let mid = b.len() / 2;
        b[mid] ^= 0x40;
        std::fs::write(&p, b).unwrap();
    }
    let promoted_bytes = std::fs::read(work.join("MANIFEST.00000002")).unwrap();
    let want = per_commit[1].clone(); // the state commit 2 acknowledged

    let mut base = Fs::default();
    base.seed_durable(&root);
    record::arm();
    let report =
        turndb::store::recover_manifest(&work, cfg, RecoveryOptions { max_rollback_commits: 2 })
            .unwrap();
    let ops = record::disarm();
    assert_eq!((report.commit, report.rollback_commits), (2, 2));
    assert!(ops.len() > 5, "the promotion must exercise a real op stream, got {}", ops.len());

    let stage = tmp("recover-stage");
    let checked = replay_recorded("recovery", &base, &root, &ops, &stage, |stage, k, variant| {
        let dir = stage.join("store");
        // The f0f59ad invariant: the two timelines are NEVER both on disk. Once the promoted
        // manifest is live, every abandoned newer retained name must already be durably gone —
        // otherwise a re-run of recovery could resurrect the abandoned history, and the next
        // flush would truncate a part those manifests still pin.
        if std::fs::read(dir.join("MANIFEST")).ok().as_deref() == Some(promoted_bytes.as_slice()) {
            for c in [3u64, 4] {
                assert!(
                    !dir.join(format!("MANIFEST.{c:08}")).exists(),
                    "crash point {k} {variant:?}: promoted MANIFEST and abandoned retained \
                     commit {c} are BOTH durable"
                );
            }
        }
        // Reopen. Refusal is legitimate only while the damaged manifest is still live — and from
        // that state, RE-RUNNING recovery must converge on the same target, never a different
        // history.
        let store = match Store::open(&dir, cfg) {
            Ok(s) => s,
            Err(_) => {
                let r = turndb::store::recover_manifest(
                    &dir,
                    cfg,
                    RecoveryOptions { max_rollback_commits: 2 },
                )
                .unwrap_or_else(|e| {
                    panic!("crash point {k} {variant:?}: recovery cannot resume: {e:#}")
                });
                assert_eq!(
                    r.commit, 2,
                    "crash point {k} {variant:?}: re-run recovery promoted a DIFFERENT commit"
                );
                Store::open(&dir, cfg).unwrap_or_else(|e| {
                    panic!("crash point {k} {variant:?}: open refused after recovery: {e:#}")
                })
            }
        };
        // Whichever path got here: the live commit is the promoted one, nothing retained
        // exceeds it, the chain verifies, and the logical state is exactly the promoted prefix.
        assert_eq!(store.manifest().commit, 2, "crash point {k} {variant:?}");
        let retained = turndb::store::retained_commits(&dir).unwrap();
        assert!(
            retained.iter().all(|&c| c <= 2),
            "crash point {k} {variant:?}: retained commit newer than live survives: {retained:?}"
        );
        turndb::store::verify_chain(&dir).unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: chain verification failed: {e:#}")
        });
        let ids = store.ids().unwrap();
        assert_eq!(
            ids,
            want.keys().cloned().collect::<Vec<_>>(),
            "crash point {k} {variant:?}: recovered ids are not the promoted prefix"
        );
        for (id, body) in &want {
            assert_eq!(
                store.reconstruct(id).unwrap().as_deref(),
                Some(body.as_slice()),
                "crash point {k} {variant:?}: promoted record {id} drifted"
            );
        }
    });
    println!("dst recovery: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// Hole punching needs `fallocate(PUNCH_HOLE)`, which only Linux provides — the same gate the
/// operation itself lives behind. Elsewhere the punch path is unreachable, so there is nothing
/// to crash-sweep; the declare-then-deallocate MANIFEST commit it shares with every other commit
/// is covered by the sweeps above.
#[cfg(target_os = "linux")]
#[test]
fn every_punch_crash_leaves_declared_blocks_retryable() {
    let root = tmp("punch");
    std::fs::create_dir_all(&root).unwrap();
    let work = root.join("store");
    // A tiny block target so the superseded piece occupies its own punchable block, and
    // INCOMPRESSIBLE content so the punched range is extent-scale (~32 KiB stored) — the size a
    // real deallocation tears at — rather than an 18-byte zstd run.
    let cfg = FoldCfg { block_target: 1, ..Default::default() };
    let mut x = 0x243F_6A88_85A3_08D3u64;
    let mut noise = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    };
    let old = noise(32 * 1024);
    let live = noise(32 * 1024);
    {
        let mut s = Store::open(&work, cfg).unwrap();
        s.put("k", &[Span::Piece(&old)], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
        s.put("k", &[Span::Piece(&live)], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
    }
    let mut base = Fs::default();
    base.seed_durable(&root);
    record::arm();
    let stats = {
        let mut s = Store::open(&work, cfg).unwrap();
        s.punch_unreferenced().unwrap()
    };
    let ops = record::disarm();
    assert!(stats.blocks_punched > 0, "the workload must actually punch, got {stats:?}");
    assert!(
        ops.iter().any(|op| matches!(op, Op::PunchHole { .. })),
        "the recording must see the punches — the vfs seam is the crash-safety argument"
    );

    let stage = tmp("punch-stage");
    let checked = replay_recorded("punch", &base, &root, &ops, &stage, |stage, k, variant| {
        let dir = stage.join("store");
        // Erasure-in-place never endangers opening: declare-before-deallocate means every hole
        // the crash left behind is already accounted for by the manifest. The sharpest state is
        // the TORN punch — fallocate landed on only part of the range before power loss, so the
        // declared block's payload is neither intact nor all zeros — and recovery must step over
        // that frame exactly as it steps over a fully-zeroed one, because the manifest's punched
        // declaration, not the payload's content, is the erasure authority.
        let mut s = Store::open(&dir, cfg).unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: open refused after a punch crash: {e:#}")
        });
        assert_eq!(
            s.reconstruct("k").unwrap().as_deref(),
            Some(live.as_slice()),
            "crash point {k} {variant:?}: live record damaged by punching dead blocks"
        );
        // A later call retries whatever was declared but not yet (or only partially)
        // deallocated; afterwards the declaration must stand and verification must pass over
        // the punched fold.
        s.punch_unreferenced()
            .unwrap_or_else(|e| panic!("crash point {k} {variant:?}: punch cannot resume: {e:#}"));
        assert!(
            !s.manifest().punched.is_empty(),
            "crash point {k} {variant:?}: resumed punch lost the punched declaration"
        );
        s.verify().unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: verification failed after resumed punch: {e:#}")
        });
        assert_eq!(s.reconstruct("k").unwrap().as_deref(), Some(live.as_slice()));
    });
    println!("dst punch: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// Decode the checked-in hex dump of the version-one consumer artifact.
fn revision_one_pack_bytes() -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("bindings/node/qualification/fixtures/revision-one.turndb.hex");
    let hex = std::fs::read_to_string(&path).unwrap();
    let digits: Vec<u8> = hex.bytes().filter(u8::is_ascii_hexdigit).collect();
    assert_eq!(digits.len() % 2, 0, "fixture hex must hold whole bytes");
    digits
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16)
                .expect("fixture holds only hex digits")
        })
        .collect()
}

#[test]
fn every_format_migration_crash_preserves_contents_and_resumes() {
    let root = tmp("migrate");
    std::fs::create_dir_all(&root).unwrap();
    let work = root.join("store");
    // Materialize the REAL version-1 artifact — not a synthetic fixture — and restore it into
    // the modeled root. The restore protocol has its own sweep above; here it is baseline.
    let pack_path = tmp("migrate-pack");
    std::fs::write(&pack_path, revision_one_pack_bytes()).unwrap();
    turndb::pack::restore(&pack_path, &work).unwrap();
    std::fs::remove_file(&pack_path).ok();
    let cfg = FoldCfg::default();
    let want: [(&str, &[u8]); 2] =
        [("legacy/0001", b"revision one request"), ("legacy/0002", b"revision one response")];

    let mut base = Fs::default();
    base.seed_durable(&root);
    record::arm();
    {
        let mut s = Store::open(&work, cfg).unwrap();
        assert_eq!(s.format_migration_status().unwrap().legacy_parts, 2, "fixture shape moved");
        // One legacy part rewritten and atomically published per step, twice, to completion.
        assert!(s.migrate_format_step().unwrap().is_some());
        assert!(s.migrate_format_step().unwrap().is_some());
        assert!(s.migrate_format_step().unwrap().is_none());
    }
    let ops = record::disarm();
    assert!(ops.len() > 20, "the migration must exercise a real op stream, got {}", ops.len());

    let stage = tmp("migrate-stage");
    let checked = replay_recorded("migration", &base, &root, &ops, &stage, |stage, k, variant| {
        let dir = stage.join("store");
        // Migration is commit-protocol work throughout: no crash point may leave a store that
        // refuses to open.
        let mut s = Store::open(&dir, cfg).unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: open refused mid-migration: {e:#}")
        });
        let check_contents = |s: &Store, when: &str| {
            for (id, bytes) in &want {
                assert_eq!(
                    s.reconstruct_content(id, BODY_CONTENT).unwrap().as_deref(),
                    Some(*bytes),
                    "crash point {k} {variant:?}: {id} drifted {when}"
                );
                let rec = s.get(id).unwrap().unwrap();
                assert_eq!(rec.contents.len(), 1, "crash point {k} {variant:?}");
                // Version-1 values carry no whole-value identity; neither migration nor crash
                // recovery may invent one — an identity is computed at ingest over the original
                // bytes or it does not exist.
                assert!(
                    rec.contents[0].identity.is_none(),
                    "crash point {k} {variant:?}: identity invented for {id} {when}"
                );
                let n = if *id == "legacy/0001" { 1 } else { 2 };
                assert_eq!(
                    rec.attrs,
                    vec![
                        ("source".to_string(), AttrValue::Str("qualification".into())),
                        ("n".to_string(), AttrValue::Int(n)),
                    ],
                    "crash point {k} {variant:?}: attrs drifted for {id} {when}"
                );
            }
        };
        check_contents(&s, "after reopen");
        // Resume to completion. Each step retires one legacy part, so the loop is bounded by
        // the fixture's part count; completion is proven by the status report, not by the loop
        // merely ending.
        let mut steps = 0usize;
        while s
            .migrate_format_step()
            .unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: migration cannot resume: {e:#}")
            })
            .is_some()
        {
            steps += 1;
            assert!(steps <= 2, "crash point {k} {variant:?}: more steps than legacy parts");
        }
        let status = s.format_migration_status().unwrap();
        assert_eq!(
            (status.legacy_parts, status.current_parts),
            (0, 2),
            "crash point {k} {variant:?}: migration did not complete"
        );
        check_contents(&s, "after completed migration");
    });
    println!("dst migration: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// A HashMap is deliberately not used for the namespaces: iteration order feeds materialization,
/// and nondeterministic iteration would make a failing seed unreproducible.
#[allow(dead_code)]
fn _model_is_deterministic(_: &HashMap<(), ()>) {}
