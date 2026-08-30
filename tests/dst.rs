//! Deterministic simulation testing: every crash the filesystem can permit, replayed for real.
//!
//! The vfs seam records every mutating operation a workload performs — bytes included — and this
//! harness reconstructs, for every operation boundary, the states a power loss could leave behind
//! under the STRICT POSIX durability model: a write is volatile until its file's fsync; a created,
//! renamed, or unlinked NAME is volatile until its parent directory's fsync; and the two are
//! independent, which is where real filesystems hide their sharpest teeth.
//!
//! Each crash state is materialized for real and opened by the real writer — the single-file
//! store for the main workload, the directory forms where a protocol sweep still tests them.
//! The invariants are absolute:
//!   * opening NEVER panics, and never refuses — every reachable crash state is a documented
//!     recovery, not an error;
//!   * every record ACKed (its `sync()` returned) before the crash point is present and
//!     byte-exact — every named content, every attribute bit, NaN payloads included — and every
//!     acked delete stays deleted;
//!   * whatever else is present is byte-exact too — a half-applied batch, a resurrected record,
//!     or drifted content is a failure even if no ack covered it.
//!
//! Beyond the mixed write path, each PUBLICATION PROTOCOL gets its own crash sweep: backup,
//! restore, manifest recovery promotion, content-hole punching, format migration, conversion, the
//! container session cycle, merge, erasure, and free-space punching each run once for real, and
//! then every op prefix × durability variant is replayed against protocol-specific invariants.
//! Container superblock alternation is also proven from each recorded trace directly — see
//! `assert_slot_alternation`.
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

/// Which platform's DOCUMENTED durability rules the crash states are derived under. Both models
/// run on every platform: a recorded op log is the same shape everywhere (Windows records
/// `SyncDir` where it publishes pending names), so the Windows proof is deterministic on Linux
/// and the POSIX proof runs on Windows.
///
/// **Posix** — strict: a write is volatile until its file's fsync; a created, renamed or unlinked
/// NAME is volatile until its parent directory's fsync.
///
/// **Windows** — built from documented operations only, nothing inferred (obj-mtfoklqo-c ruling):
///   * there is no directory fsync; `SyncDir` is the point where the engine publishes every file
///     it created in that directory with `MoveFileExW(MOVEFILE_WRITE_THROUGH)` (src/vfs.rs), and
///     the model treats it as exactly those renames — a create is otherwise never durable;
///   * a rename is durable when the call returns (write-through), and a crash *during* it admits
///     old, new, or NEITHER, because Microsoft documents what a successful call did and nothing
///     about the crash state — `crash_states` below adds the neither state at every rename and
///     at every publish;
///   * an unlink has no write-through form and is never durable in the model: a deleted name may
///     be back after a crash, and stays back until a later rename or publish takes the name;
///   * a directory or a hard link is created at its name and never published — the engine
///     publishes files only — so in the model neither is ever durable (a laggable name);
///   * `SyncFile` on a file makes that file's own bytes and length durable and nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Model {
    Posix,
    Windows,
}

const MODELS: &[Model] = &[Model::Posix, Model::Windows];

/// The whole tree: an inode table plus TWO namespaces. `volatile` is what a process sees;
/// `durable` is what survives a power loss under the model — names move between them at the
/// parent directory's fsync (Posix) or at write-through renames and publishes (Windows).
#[derive(Clone)]
struct Fs {
    model: Model,
    inodes: Vec<Inode>,
    kinds: Vec<Kind>,
    volatile_ns: BTreeMap<PathBuf, usize>,
    durable_ns: BTreeMap<PathBuf, usize>,
    /// Windows only: names created (file, dir, link) and not yet published, in creation order.
    pending: Vec<PathBuf>,
}

impl Fs {
    fn new(model: Model) -> Fs {
        Fs {
            model,
            inodes: Vec::new(),
            kinds: Vec::new(),
            volatile_ns: BTreeMap::new(),
            durable_ns: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    fn windows(&self) -> bool {
        self.model == Model::Windows
    }

    /// Names directly inside `dir` awaiting publication, in creation order (Windows).
    fn pending_in(&self, dir: &Path) -> Vec<PathBuf> {
        self.pending.iter().filter(|p| p.parent() == Some(dir)).cloned().collect()
    }

    /// Publish one pending name: its dirent is now on disk (write-through rename returned).
    fn publish(&mut self, path: &Path) {
        if let Some(&i) = self.volatile_ns.get(path) {
            self.durable_ns.insert(path.to_path_buf(), i);
        }
        self.pending.retain(|p| p != path);
    }

    /// Erase every trace of `path` (and, for a directory, everything under it) from both
    /// namespaces: the "neither" state of a rename or publish that was in flight.
    fn vanish(&mut self, path: &Path) {
        for ns in [&mut self.volatile_ns, &mut self.durable_ns] {
            let doomed: Vec<PathBuf> = ns.keys().filter(|p| p.starts_with(path)).cloned().collect();
            for p in doomed {
                ns.remove(&p);
            }
        }
        self.pending.retain(|p| !p.starts_with(path));
    }

    /// The crash states reachable when the crash lands ON `next` rather than before it. Every
    /// model has the "before" state (the prefix already applied). The Windows model adds, for a
    /// rename, the state where neither name exists; for a publish (`SyncDir`), one state per
    /// pending file where the files before it were published and that one is gone.
    fn crash_states(&self, next: Option<&Op>) -> Vec<(Fs, String)> {
        let mut out = vec![(self.clone(), String::new())];
        if !self.windows() {
            return out;
        }
        match next {
            Some(Op::Rename { from, to }) => {
                let mut fs = self.clone();
                fs.vanish(from);
                fs.vanish(to);
                out.push((fs, format!("rename-neither {}", short_name(to))));
            }
            Some(Op::SyncDir { path }) => {
                let due = self.pending_in(path);
                for (j, victim) in due.iter().enumerate() {
                    let mut fs = self.clone();
                    for p in &due[..j] {
                        fs.publish(p);
                    }
                    fs.vanish(victim);
                    out.push((fs, format!("publish-neither {}", short_name(victim))));
                }
            }
            _ => {}
        }
        out
    }

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
                // `File::create` on a name that already exists truncates THAT file in place:
                // the same inode, its name as durable as it was, only its bytes volatile until
                // its fsync — on every platform, and on Windows without entering `pending`
                // (vfs::create registers only a genuinely new name). A new name is a new inode,
                // volatile until its directory's fsync (Posix) or its publish (Windows).
                if let Some(&i) = self.volatile_ns.get(path) {
                    let node = &mut self.inodes[i];
                    node.volatile.clear();
                    node.pending.push((u64::MAX, Vec::new()));
                } else {
                    let i = self.new_inode(Kind::File);
                    self.volatile_ns.insert(path.clone(), i);
                    if self.windows() {
                        self.pending.push(path.clone());
                    }
                }
            }
            Op::WriteFile { path, data } => {
                // `std::fs::write` likewise: an existing name is rewritten in place (modelled as
                // a rewrite to this image, like truncation); a new name is a new inode.
                if let Some(&i) = self.volatile_ns.get(path) {
                    let node = &mut self.inodes[i];
                    node.volatile = data.clone();
                    node.pending.push((u64::MAX, data.clone()));
                } else {
                    let i = self.new_inode(Kind::File);
                    self.volatile_ns.insert(path.clone(), i);
                    let node = &mut self.inodes[i];
                    node.pending.push((0, data.clone()));
                    node.volatile = data.clone();
                    if self.windows() {
                        self.pending.push(path.clone());
                    }
                }
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
            Op::SyncDir { path } if self.windows() => {
                // Windows: no directory fsync exists; this is the engine publishing each pending
                // name in `path` with a write-through rename. Only those names become durable —
                // not unlinks, and not anything else in the directory.
                for p in self.pending_in(path) {
                    self.publish(&p);
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
                    if self.windows() {
                        // Write-through: the new name is on disk when the call returns, and the
                        // old one is gone from disk with it. A pending source is published by
                        // being renamed.
                        self.durable_ns.remove(from);
                        self.durable_ns.insert(to.clone(), i);
                        self.pending.retain(|p| p != from);
                    }
                }
            }
            Op::Link { from, to } => {
                // Windows: never published by the engine, so never durable in the model.
                if let Some(&i) = self.volatile_ns.get(from) {
                    self.volatile_ns.insert(to.clone(), i);
                }
            }
            Op::Unlink { path } => {
                self.volatile_ns.remove(path);
                // Windows: DeleteFile has no write-through form, so the durable name stays until
                // something else takes it. An unpublished temp that is deleted simply never was.
                if self.windows() {
                    self.pending.retain(|p| p != path);
                }
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
                // Windows: a directory is created at its name and never published (the engine
                // publishes files only), so its name is never durable in the model.
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
    /// TornTail's cut, but at a fraction of the write's first 64 bytes rather than its whole
    /// length. Length-proportional cuts never land inside the defined region of a page-sized
    /// write — for a 4 KiB superblock slot, thirds fall at 1365 and 2730 while everything the
    /// format defines sits in the first 56 bytes, so every TornTail leaves the slot's claim
    /// intact and checksum-valid. This variant cuts at 21 and 42: through the sequence, the
    /// directory pointer, and the tail, which is what forces the slot checksum to actually
    /// carry the crash-safety argument.
    TornHead(u8),
    /// Only the LAST pending write to each file landed; every earlier unsynced write to that file
    /// did not.
    ///
    /// Without an intervening fsync the order dirty pages reach the platter is unspecified, so a
    /// later write surviving while an earlier one is lost is a legal POSIX outcome — and it is the
    /// precise hazard a commit record poses. A protocol that writes its commit record last and
    /// fsyncs before it is unaffected: at every crash point the record is either not yet written,
    /// or everything it names is already durable. A protocol that skips that fsync publishes a
    /// pointer to bytes that never landed, and only this variant can tell the two apart.
    LastPendingOnly,
}

const VARIANTS: &[Variant] = &[
    Variant::DurableOnly,
    Variant::AllLanded,
    Variant::NamesLag,
    Variant::ContentLag,
    Variant::TornTail(1),
    Variant::TornTail(2),
    Variant::TornHead(1),
    Variant::TornHead(2),
    Variant::LastPendingOnly,
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
        Variant::TornTail(frac) | Variant::TornHead(frac) => fs
            .inodes
            .iter()
            .enumerate()
            .rev()
            .find(|(_, n)| n.pending.last().is_some_and(|(off, _)| *off != u64::MAX))
            .map(|(i, n)| {
                let (_, data) = n.pending.last().unwrap();
                let span = match variant {
                    Variant::TornHead(_) => data.len().min(64),
                    _ => data.len(),
                };
                (i, span * frac as usize / 3)
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
                    Variant::LastPendingOnly => {
                        let mut img = node.durable.clone();
                        if let Some((off, data)) = node.pending.last() {
                            if *off == u64::MAX {
                                img = data.clone();
                            } else {
                                let end = *off as usize + data.len();
                                if img.len() < end {
                                    img.resize(end, 0);
                                }
                                img[*off as usize..end].copy_from_slice(data);
                            }
                        }
                        img
                    }
                    Variant::TornTail(_) | Variant::TornHead(_) => {
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

/// The model follows the implementation's existing-name branch: creating or rewriting a name
/// that already exists changes that inode's bytes in place — durable bytes and the durable
/// name untouched until the file's own fsync — and, on Windows, never enters `pending`, so a
/// crash on the directory sync has no publish-neither state for it.
#[test]
fn the_model_treats_create_and_write_on_an_existing_name_as_in_place_content_changes() {
    for &model in MODELS {
        let root = PathBuf::from("/m");
        let name = root.join("existing");
        let mut fs = Fs::new(model);
        // Seed: a durable directory and a durable file with old bytes.
        let d = fs.new_inode(Kind::Dir);
        fs.volatile_ns.insert(root.clone(), d);
        fs.durable_ns.insert(root.clone(), d);
        let i = fs.new_inode(Kind::File);
        fs.inodes[i].durable = b"old bytes".to_vec();
        fs.inodes[i].volatile = b"old bytes".to_vec();
        fs.volatile_ns.insert(name.clone(), i);
        fs.durable_ns.insert(name.clone(), i);

        // Create (truncate) then write: same inode, durable untouched, nothing pending.
        fs.apply(&Op::Create { path: name.clone() });
        fs.apply(&Op::WriteAt { path: name.clone(), off: 0, data: b"new".to_vec() });
        assert_eq!(fs.volatile_ns[&name], i, "{model:?}: the same inode");
        assert_eq!(fs.inodes[i].durable, b"old bytes", "{model:?}: durable bytes unchanged");
        assert_eq!(fs.inodes[i].volatile, b"new", "{model:?}: volatile bytes truncated+written");
        assert_eq!(fs.durable_ns.get(&name), Some(&i), "{model:?}: the name stays durable");
        assert!(fs.pending.is_empty(), "{model:?}: an existing name never enters pending");
        // A crash on the directory sync has no publish-neither state for it.
        let states = fs.crash_states(Some(&Op::SyncDir { path: root.clone() }));
        assert_eq!(
            states.len(),
            1,
            "{model:?}: {:?}",
            states.iter().map(|(_, l)| l).collect::<Vec<_>>()
        );
        // After the file's fsync the bytes are durable; the name never changed.
        fs.apply(&Op::SyncFile { path: name.clone() });
        assert_eq!(fs.inodes[i].durable, b"new", "{model:?}");
        assert_eq!(fs.durable_ns.get(&name), Some(&i), "{model:?}");
        // WriteFile on the existing name: also in place.
        fs.apply(&Op::WriteFile { path: name.clone(), data: b"rewritten".to_vec() });
        assert_eq!(fs.volatile_ns[&name], i, "{model:?}");
        assert_eq!(fs.inodes[i].durable, b"new", "{model:?}: durable until fsync");
        assert!(fs.pending.is_empty(), "{model:?}");
        // A NEW name behaves as before: new inode; pending on Windows; publish-neither exists.
        let fresh = root.join("fresh");
        fs.apply(&Op::Create { path: fresh.clone() });
        assert_ne!(fs.volatile_ns[&fresh], i);
        assert_eq!(fs.durable_ns.get(&fresh), None, "{model:?}: not durable before a barrier");
        let states = fs.crash_states(Some(&Op::SyncDir { path: root.clone() }));
        assert_eq!(states.len(), if model == Model::Windows { 2 } else { 1 }, "{model:?}");
    }
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
    // The parent directory is the given world, not part of the recorded protocol — the store
    // FILE and its sidecar are what the crash model owns.
    std::fs::create_dir_all(dir).unwrap();
    record::arm();
    let mut issued: Vec<Issued> = Vec::new();
    let mut acks: Vec<Ack> = Vec::new();
    let mut group = 0usize;

    {
        let mut s = Store::open_file(&dir.join("s.turndb"), cfg).unwrap();
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
        let mut s = Store::open_file(&dir.join("s.turndb"), cfg).unwrap();
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
    assert_slot_alternation("workload", &ops);
    let boundaries = group_boundaries(&issued);

    let stage = root.join("stage");
    let mut checked = 0usize;
    for &model in MODELS {
        for k in 0..=ops.len() {
            // The ack floor: the largest issued prefix whose sync completed within the op
            // prefix. Recovery may sit ANYWHERE at or beyond it (later writes were issued, just
            // not acked), but always at a group boundary and always exactly a prefix — no holes,
            // no reordering.
            let floor: usize =
                acks.iter().rev().find(|(n, _)| *n <= k).map(|(_, p)| *p).unwrap_or(0);

            let mut fs = Fs::new(model);
            for op in &ops[..k] {
                fs.apply(op);
            }

            for (fs, label) in fs.crash_states(ops.get(k)) {
                for &variant in VARIANTS {
                    if !materialize(&fs, variant, &work, &stage) {
                        continue; // nothing durable yet — an empty directory is a new store
                    }
                    let r = catch_unwind(AssertUnwindSafe(|| {
                        check_state(&stage, &issued, &boundaries, floor, k, variant)
                    }));
                    if r.is_err() {
                        eprintln!("--- {model:?} {label}: op trace up to crash point {k} ---");
                        for (i, op) in ops[..k].iter().enumerate() {
                            eprintln!("{i:4}: {}", op_summary(op));
                        }
                        panic!(
                            "FAILED under {model:?} {label} at crash point {k} variant {variant:?}"
                        );
                    }
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 0);
    println!("dst: {} crash states checked across {} ops, both models", checked, ops.len());
    std::fs::remove_dir_all(&root).ok();
}

fn short_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
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
        if let Ok(rs) = turndb::store::open_read_container(&stage.join("s.turndb"), cfg) {
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
    let store = match Store::open_file(&stage.join("s.turndb"), cfg) {
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
    for &model in MODELS {
        for k in 0..=ops.len() {
            let mut fs = base.clone();
            fs.model = model;
            for op in &ops[..k] {
                fs.apply(op);
            }
            for (fs, label) in fs.crash_states(ops.get(k)) {
                for &variant in VARIANTS {
                    materialize(&fs, variant, root, stage);
                    let r = catch_unwind(AssertUnwindSafe(|| check(stage, k, variant)));
                    if r.is_err() {
                        eprintln!(
                            "--- {tag} {model:?} {label}: op trace up to crash point {k} ---"
                        );
                        for (i, op) in ops[..k].iter().enumerate() {
                            eprintln!("{i:4}: {}", op_summary(op));
                        }
                        panic!(
                            "{tag}: FAILED under {model:?} {label} at crash point {k} \
                             variant {variant:?}"
                        );
                    }
                    checked += 1;
                }
            }
        }
    }
    checked
}

/// Slot alternation, proven from the trace rather than hoped for from a crash.
///
/// The crash sweep alone cannot establish that superblock writes alternate: a slot overwritten in
/// place still passes every crash state whose tear spares its checksummed prefix, and the reader
/// race alternation exists for — resolving a slot while the writer rewrites it — is a race no
/// crash model expresses. But the invariant is a property of the write *sequence*, so it is
/// checked on the recorded op log directly: consecutive superblock claims (magic-bearing,
/// whole-slot positioned writes at slot offsets) on one file may never target the same slot,
/// deterministically, at every recording.
fn assert_slot_alternation(tag: &str, ops: &[Op]) {
    let slot_len = turndb::container::SLOT_LEN;
    let mut last: BTreeMap<&Path, u64> = BTreeMap::new();
    for op in ops {
        let Op::WriteAt { path, off, data } = op else { continue };
        if (*off == 0 || *off == slot_len)
            && data.len() as u64 == slot_len
            && data.starts_with(turndb::container::MAGIC)
        {
            if let Some(prev) = last.insert(path.as_path(), *off) {
                assert_ne!(
                    prev,
                    *off,
                    "{tag}: {} wrote a superblock claim into the slot the previous claim \
                     occupies — the live slot was overwritten instead of alternated",
                    path.display()
                );
            }
        }
    }
}

/// A small settled single-file store at `file`, closed cleanly, plus the id -> body map it must
/// serve.
fn build_settled_file_store(file: &Path, cfg: FoldCfg, tag: usize) -> BTreeMap<String, Vec<u8>> {
    let mut want = BTreeMap::new();
    let mut s = Store::open_file(file, cfg).unwrap();
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
    s.close().unwrap();
    want
}

/// Flip one byte inside a member of a single-file store, without truncating anything.
fn flip_member_byte(store: &Path, name: &str, at_frac: f32) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let (off, len) = {
        let c = turndb::container::Container::open(store).unwrap();
        c.member_extents(name).unwrap()[0]
    };
    let at = off + ((len as f32 * at_frac) as u64).min(len - 1);
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(store).unwrap();
    f.seek(SeekFrom::Start(at)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    b[0] ^= 0x40;
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&b).unwrap();
    f.sync_all().unwrap();
}

/// Materialize the checked-in 0.1.3 directory-store fixture: the retired layout exactly as its
/// last writer left it, non-empty WAL included. The conversion sweep drives the ONE door the
/// layout has left, so its input must be an artifact this codebase can no longer produce.
fn unpack_dir_fixture(into: &Path) {
    let hex_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/directory-store-0.1.3.hex");
    let text = std::fs::read_to_string(&hex_path).unwrap();
    let mut name: Option<std::path::PathBuf> = None;
    let mut hex = String::new();
    let flush = |name: &Option<std::path::PathBuf>, hex: &str| {
        if let Some(path) = name {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    };
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("== ") {
            flush(&name, &hex);
            hex.clear();
            name = Some(into.join(rest.split_whitespace().next().unwrap()));
        } else {
            hex.push_str(line.trim());
        }
    }
    flush(&name, &hex);
}

/// The fixture generator's body function, byte for byte (xorshift64 over the seed).
fn fixture_body(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

/// Every id -> body the converted 0.1.3 fixture must serve: two flushed rounds minus the
/// deleted record, plus the WAL-only record whose replay is the conversion's hardest promise.
fn fixture_expectations() -> BTreeMap<String, Vec<u8>> {
    let mut want = BTreeMap::new();
    for round in 0..2u64 {
        for i in 0..6u64 {
            let id = format!("fix:{round}:{i}");
            if id == "fix:0:0" {
                continue;
            }
            let mut body = b"[".to_vec();
            body.extend_from_slice(&fixture_body(round * 10 + i, 1800));
            body.extend_from_slice(b"]");
            want.insert(id, body);
        }
    }
    want.insert("fix:wal:only".to_string(), fixture_body(999, 700));
    want
}

/// A backup of a single-file store is a sealed container published by a no-replace rename. The
/// sweep's claim: the SOURCE file answers identically at every crash point, and the artifact is
/// all-or-nothing at its final name — absent, or complete, sealed, and byte-exact.
#[test]
fn every_backup_crash_leaves_the_source_intact_and_the_artifact_all_or_nothing() {
    let root = tmp("backup");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("store.turndb");
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    let want = build_settled_file_store(&file, cfg, 200);

    let out = root.join("backup.turndb");
    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
        s.backup(&out).unwrap();
        s.close().unwrap();
    }
    let ops = record::disarm();
    assert!(ops.len() > 8, "the backup must exercise a real op stream, got {}", ops.len());
    assert_slot_alternation("backup", &ops);

    let stage = tmp("backup-stage");
    let checked = replay_recorded("backup", &base, &root, &ops, &stage, |stage, k, variant| {
        // The SOURCE reopens consistent no matter where the export died: backup is read-only
        // with respect to the store's logical state.
        let src = Store::open_file(&stage.join("store.turndb"), cfg).unwrap_or_else(|e| {
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
        // The ARTIFACT is all-or-nothing at its final name: absent, or a complete SEALED
        // container that verifies and serves every record byte-exact. (Staging litter beside
        // it is allowed — it is inert bytes at a name nothing resolves.)
        let dst = stage.join("backup.turndb");
        if dst.exists() {
            let c = turndb::container::Container::open(&dst).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: a torn artifact sits at the FINAL name: {e:#}")
            });
            assert!(c.sealed(), "crash point {k} {variant:?}: a published backup must be sealed");
            c.verify().unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: published backup fails verification: {e:#}")
            });
            drop(c);
            let rs = turndb::store::open_read_container(&dst, cfg).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: published backup refuses a reader: {e:#}")
            });
            for (id, body) in &want {
                assert_eq!(
                    rs.reconstruct(id).unwrap().as_deref(),
                    Some(body.as_slice()),
                    "crash point {k} {variant:?}: backup record {id} drifted"
                );
            }
        }
    });
    println!("dst backup: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// Restoring is member-verified copying of a sealed backup into a fresh writable file, staged
/// and published by a no-replace rename. The destination is all-or-nothing at its final name,
/// and a crash is always recoverable by simply re-running the restore.
#[test]
fn every_restore_crash_leaves_the_destination_all_or_nothing() {
    let root = tmp("restore");
    std::fs::create_dir_all(&root).unwrap();
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    // The source store lives OUTSIDE the modeled root: only the sealed artifact and the restore
    // protocol itself are under test here.
    let srcroot = tmp("restore-src");
    std::fs::create_dir_all(&srcroot).unwrap();
    let srcfile = srcroot.join("src.turndb");
    let want = build_settled_file_store(&srcfile, cfg, 400);
    let origin = root.join("origin.turndb");
    {
        let mut s = Store::open_file(&srcfile, cfg).unwrap();
        s.backup(&origin).unwrap();
        s.close().unwrap();
    }
    std::fs::remove_dir_all(&srcroot).ok();

    let dest = root.join("restored.turndb");
    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    turndb::store::restore_file(&origin, &dest).unwrap();
    let ops = record::disarm();
    assert!(ops.len() > 4, "the restore must exercise a real op stream, got {}", ops.len());

    let stage = tmp("restore-stage");
    let checked = replay_recorded("restore", &base, &root, &ops, &stage, |stage, k, variant| {
        let dst = stage.join("restored.turndb");
        if dst.exists() {
            // Published means COMPLETE: the final name only ever appears via the no-replace
            // rename of a fully verified, unsealed staging copy.
            let c = turndb::container::Container::open(&dst).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: a partial file sits at the FINAL name: {e:#}")
            });
            assert!(
                !c.sealed(),
                "crash point {k} {variant:?}: the restored copy must be born writable"
            );
            c.verify().unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: restored store fails verification: {e:#}")
            });
            drop(c);
            let rs = turndb::store::open_read_container(&dst, cfg).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: restored store refuses a reader: {e:#}")
            });
            for (id, body) in &want {
                assert_eq!(
                    rs.reconstruct(id).unwrap().as_deref(),
                    Some(body.as_slice()),
                    "crash point {k} {variant:?}: restored record {id} drifted"
                );
            }
            assert_eq!(rs.ids().unwrap().len(), want.len());
        } else {
            // No destination: the crash must be recoverable by simply RE-RUNNING the restore.
            // Staging litter is allowed to remain and must not block the retry.
            turndb::store::restore_file(&stage.join("origin.turndb"), &dst).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: restore cannot be re-run: {e:#}")
            });
            let rs = turndb::store::open_read_container(&dst, cfg).unwrap();
            for (id, body) in &want {
                assert_eq!(rs.reconstruct(id).unwrap().as_deref(), Some(body.as_slice()));
            }
        }
    });
    println!("dst restore: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// Checked recovery of a single-file store promotes a retained manifest with ONE slot flip —
/// so the abandoned timeline and the promoted one are never both durable by construction, and
/// every crash inside recovery converges on the same promoted commit when re-run.
#[test]
fn every_recovery_crash_converges_on_the_promoted_timeline() {
    let root = tmp("recover");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("s.turndb");
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    // Four commits, tracking the exact logical state at each, so "the promoted prefix" is a
    // concrete map rather than a mood.
    let mut per_commit: Vec<BTreeMap<String, Vec<u8>>> = Vec::new();
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
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
        s.close().unwrap();
    }
    // Damage the live manifest member AND the two newest retained copies — the shape checked
    // recovery exists for. The damage is the operator's starting point, not a protocol step, so
    // it is baseline state rather than recorded ops: what gets crash-swept is recovery itself.
    for name in ["MANIFEST", "MANIFEST.00000004", "MANIFEST.00000003"] {
        flip_member_byte(&file, name, 0.5);
    }
    let promoted_bytes = {
        let c = turndb::container::Container::open(&file).unwrap();
        c.read_file_bounded("MANIFEST.00000002", turndb::store::MAX_MANIFEST_BYTES).unwrap()
    };
    let want = per_commit[1].clone(); // the state commit 2 acknowledged

    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    let report = turndb::store::recover_manifest_file(
        &file,
        cfg,
        RecoveryOptions { max_rollback_commits: 2 },
    )
    .unwrap();
    let ops = record::disarm();
    assert_eq!((report.commit, report.rollback_commits), (2, 2));
    assert!(ops.len() > 2, "the promotion must exercise a real op stream, got {}", ops.len());
    assert_slot_alternation("recovery", &ops);

    let stage = tmp("recover-stage");
    let checked = replay_recorded("recovery", &base, &root, &ops, &stage, |stage, k, variant| {
        let f = stage.join("s.turndb");
        // The two timelines are never both durable: promotion IS the flip that installs the
        // promoted manifest and drops the abandoned retained members, atomically.
        let c = turndb::container::Container::open(&f).unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: container refused to open: {e:#}")
        });
        if c.read_file_bounded("MANIFEST", turndb::store::MAX_MANIFEST_BYTES).ok().as_deref()
            == Some(promoted_bytes.as_slice())
        {
            for commit in [3u64, 4] {
                assert!(
                    !c.contains(&format!("MANIFEST.{commit:08}")),
                    "crash point {k} {variant:?}: promoted MANIFEST and abandoned retained \
                     commit {commit} are BOTH durable"
                );
            }
        }
        drop(c);
        // Reopen. Refusal is legitimate only while the damaged manifest is still live — and from
        // that state, RE-RUNNING recovery must converge on the same target, never a different
        // history.
        let store = match Store::open_file(&f, cfg) {
            Ok(s) => s,
            Err(_) => {
                let r = turndb::store::recover_manifest_file(
                    &f,
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
                Store::open_file(&f, cfg).unwrap_or_else(|e| {
                    panic!("crash point {k} {variant:?}: open refused after recovery: {e:#}")
                })
            }
        };
        // Whichever path got here: the live commit is the promoted one, nothing retained
        // exceeds it, the chain verifies, and the logical state is exactly the promoted prefix.
        assert_eq!(store.manifest().commit, 2, "crash point {k} {variant:?}");
        drop(store);
        let retained = turndb::store::retained_commits_file(&f).unwrap();
        assert!(
            retained.iter().all(|&c| c <= 2),
            "crash point {k} {variant:?}: retained commit newer than live survives: {retained:?}"
        );
        turndb::store::verify_chain_file(&f).unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: chain verification failed: {e:#}")
        });
        let rs = turndb::store::open_read_container(&f, cfg).unwrap();
        let ids = rs.ids().unwrap();
        assert_eq!(
            ids,
            want.keys().cloned().collect::<Vec<_>>(),
            "crash point {k} {variant:?}: recovered ids are not the promoted prefix"
        );
        for (id, body) in &want {
            assert_eq!(
                rs.reconstruct(id).unwrap().as_deref(),
                Some(body.as_slice()),
                "crash point {k} {variant:?}: promoted record {id} drifted"
            );
        }
    });
    println!("dst recovery: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

#[cfg(target_os = "linux")]
#[test]
fn every_punch_crash_leaves_declared_blocks_retryable() {
    let root = tmp("punch");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("live.turndb");
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
        let mut s = Store::open_file(&file, cfg).unwrap();
        s.put("k", &[Span::Piece(&old)], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
        s.put("k", &[Span::Piece(&live)], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
        s.close().unwrap();
    }
    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    let stats = {
        let mut s = Store::open_file(&file, cfg).unwrap();
        s.punch_unreferenced().unwrap()
    };
    let ops = record::disarm();
    assert!(stats.blocks_punched > 0, "the workload must actually punch, got {stats:?}");
    assert!(
        ops.iter().any(|op| matches!(op, Op::PunchHole { .. })),
        "the recording must see the punches — the vfs seam is the crash-safety argument"
    );
    assert_slot_alternation("punch", &ops);

    let stage = tmp("punch-stage");
    let checked = replay_recorded("punch", &base, &root, &ops, &stage, |stage, k, variant| {
        let f = stage.join("live.turndb");
        // Erasure-in-place never endangers opening: declare-before-deallocate means every hole
        // the crash left behind is already accounted for by the manifest. The sharpest state is
        // the TORN punch — fallocate landed on only part of the range before power loss, so the
        // declared block's payload is neither intact nor all zeros — and recovery must step over
        // that frame exactly as it steps over a fully-zeroed one, because the manifest's punched
        // declaration, not the payload's content, is the erasure authority.
        let mut s = Store::open_file(&f, cfg).unwrap_or_else(|e| {
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
    let work = root.join("legacy.turndb");
    // Materialize the REAL version-1 artifact — not a synthetic fixture — and convert it into
    // the modeled root. The conversion protocol has its own sweep below; here it is baseline.
    let pack_path = tmp("migrate-pack");
    std::fs::write(&pack_path, revision_one_pack_bytes()).unwrap();
    turndb::store::convert_to_file(&pack_path, &work).unwrap();
    std::fs::remove_file(&pack_path).ok();
    let cfg = FoldCfg::default();
    let want: [(&str, &[u8]); 2] =
        [("legacy/0001", b"revision one request"), ("legacy/0002", b"revision one response")];

    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    {
        let mut s = Store::open_file(&work, cfg).unwrap();
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
        let dir = stage.join("legacy.turndb");
        // Migration is commit-protocol work throughout: no crash point may leave a store that
        // refuses to open.
        let mut s = Store::open_file(&dir, cfg).unwrap_or_else(|e| {
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

/// Conversion is the retired directory layout's ONE remaining door, and its input is by
/// definition an artifact this codebase can no longer produce — so the sweep drives it over the
/// checked-in 0.1.3 fixture, unsettled WAL included. The claim: the destination is
/// all-or-nothing at its final name, and every crash state — including any state the settle
/// left the SOURCE directory in — is recovered by simply re-running the conversion.
#[test]
fn every_conversion_crash_is_recovered_by_rerunning_the_conversion() {
    let root = tmp("convert");
    std::fs::create_dir_all(&root).unwrap();
    let work = root.join("store");
    unpack_dir_fixture(&work);
    let want = fixture_expectations();
    let cfg = FoldCfg::default();
    let container = root.join("state.turndb");

    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    turndb::store::convert_to_file(&work, &container).unwrap();
    let ops = record::disarm();
    assert!(ops.len() > 10, "the conversion must exercise a real op stream, got {}", ops.len());
    assert_slot_alternation("conversion", &ops);

    let stage = tmp("convert-stage");
    let checked = replay_recorded("conversion", &base, &root, &ops, &stage, |stage, k, variant| {
        let file = stage.join("state.turndb");
        if !file.exists() {
            // The crash landed before publication: re-running the conversion is the whole
            // recovery story — the settle resumes from whatever state the source directory is
            // in (its own recovery included), and stale staging must never block the retry.
            turndb::store::convert_to_file(&stage.join("store"), &file).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: conversion cannot be re-run: {e:#}")
            });
        }
        // Published means COMPLETE: a whole, verified container serving the fixture's entire
        // contents — the WAL-only record included, because the settle replays it before the
        // copy walks the members.
        let c = turndb::container::Container::open(&file).unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: a torn file sits at the FINAL name: {e:#}")
        });
        c.verify().unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: converted store fails verification: {e:#}")
        });
        drop(c);
        let rs = turndb::store::open_read_container(&file, cfg).unwrap_or_else(|e| {
            panic!("crash point {k} {variant:?}: converted store refuses a reader: {e:#}")
        });
        assert_eq!(
            rs.ids().unwrap().len(),
            want.len(),
            "crash point {k} {variant:?}: converted store gained or lost records"
        );
        for (id, body) in &want {
            assert_eq!(
                rs.reconstruct(id).unwrap().as_deref(),
                Some(body.as_slice()),
                "crash point {k} {variant:?}: converted record {id} drifted"
            );
        }
    });
    println!("dst conversion: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// The NATIVE single-file session: `Store::open_file`, writes, an ACK, a flush — the protocol
/// whose superblock flip is the linearization point — more writes, another ACK that is never
/// flushed, and a session end with the `-wal` sidecar left behind. Every crash state must reopen
/// from the file plus that sidecar alone: opening never refuses, an acknowledged record that
/// comes back is byte-exact, and nothing committed before the session can vanish. This is the
/// sweep the hot-directory session sweep retires into once the bridge is deleted.
#[test]
fn every_single_file_session_crash_keeps_every_acknowledged_write() {
    let root = tmp("native-session");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("live.turndb");
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    let body_for = |i: usize| -> Vec<u8> {
        let mut b = Vec::with_capacity(600);
        let mut seed = [i as u8; 32];
        while b.len() < 600 {
            seed = blake3::hash(&seed).into();
            b.extend_from_slice(&seed);
        }
        b.truncate(600);
        b
    };

    // A committed baseline from an earlier, cleanly closed session: exactly one file at rest.
    let mut before = BTreeMap::new();
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
        for i in 0..5 {
            let id = format!("before:{i}");
            let body = body_for(700 + i);
            s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
            before.insert(id, body);
        }
        s.sync().unwrap();
        s.flush().unwrap();
        s.close().unwrap();
    }
    assert!(!wal_of(&file).exists(), "a clean close leaves only the file");

    let mut acked = before.clone();
    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
        for i in 0..3 {
            let id = format!("acked:a{i}");
            let body = body_for(800 + i);
            s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
            acked.insert(id, body);
        }
        s.sync().unwrap(); // ACK — durable in the sidecar from here
        s.flush().unwrap(); // the flip: fold delta, part, manifests, one barrier, one slot
        for i in 0..3 {
            let id = format!("acked:b{i}");
            let body = body_for(900 + i);
            s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
            acked.insert(id, body);
        }
        s.sync().unwrap(); // ACKed and never flushed: the sidecar alone carries these
        drop(s); // the session dies without closing — the -wal stays behind
    }
    let ops = record::disarm();
    assert!(ops.len() > 20, "a session must exercise a real op stream, got {}", ops.len());
    assert_slot_alternation("single-file session", &ops);

    let stage = tmp("native-session-stage");
    let checked =
        replay_recorded("single-file-session", &base, &root, &ops, &stage, |stage, k, variant| {
            let file = stage.join("live.turndb");
            if !file.exists() {
                return;
            }
            let s = Store::open_file(&file, cfg).unwrap_or_else(|e| {
                panic!(
                    "crash point {k} {variant:?}: the single-file store refused to reopen: {e:#}"
                )
            });
            let mut found = 0usize;
            for (id, body) in &acked {
                match s.reconstruct(id).unwrap() {
                    Some(got) => {
                        assert_eq!(
                            got, *body,
                            "crash point {k} {variant:?}: {id} came back but drifted"
                        );
                        found += 1;
                    }
                    None => assert!(
                        !before.contains_key(id),
                        "crash point {k} {variant:?}: {id} predates this session and vanished"
                    ),
                }
            }
            assert!(
                found >= before.len(),
                "crash point {k} {variant:?}: recovered {found} records, fewer than the {} \
                 committed before the session began",
                before.len()
            );
        });
    println!("dst single-file session: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// The native merge: answer-preserving by definition, so the sweep's invariant is total — at
/// EVERY crash state, every record answers exactly as it did before the merge began. There is no
/// window where the store may serve anything else: the splice publishes in one flip, and until
/// it does, the merged member is uncommitted noise.
#[test]
fn every_single_file_merge_crash_answers_identically() {
    let root = tmp("native-merge");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("live.turndb");
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    let body_for = |i: usize| -> Vec<u8> {
        let mut b = Vec::with_capacity(500);
        let mut seed = [i as u8; 32];
        while b.len() < 500 {
            seed = blake3::hash(&seed).into();
            b.extend_from_slice(&seed);
        }
        b.truncate(500);
        b
    };

    // Three parts with overlapping ids and one delete — versions to supersede, a tombstone to
    // carry, and a mergeable run.
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
        for round in 0..3usize {
            for i in 0..6usize {
                let id = format!("m:{:02}", (round * 3 + i) % 9);
                s.put(&id, &[Span::Piece(&body_for(round * 50 + i))], vec![]).unwrap();
            }
            s.sync().unwrap();
            s.flush().unwrap();
        }
        s.delete("m:02").unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
        s.close().unwrap();
    }
    let oracle: BTreeMap<String, Option<Vec<u8>>> = {
        let s = Store::open_file(&file, cfg).unwrap();
        (0..9usize)
            .map(|i| {
                let id = format!("m:{i:02}");
                let v = s.reconstruct(&id).unwrap();
                (id, v)
            })
            .collect()
    };

    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
        s.merge_range(0, 4).unwrap().expect("four parts merge");
        s.close().unwrap();
    }
    let ops = record::disarm();
    assert!(ops.len() > 4, "a merge must exercise a real op stream, got {}", ops.len());
    assert_slot_alternation("single-file merge", &ops);

    let stage = tmp("native-merge-stage");
    let checked =
        replay_recorded("single-file-merge", &base, &root, &ops, &stage, |stage, k, variant| {
            let file = stage.join("live.turndb");
            if !file.exists() {
                return;
            }
            let s = Store::open_file(&file, cfg).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: the merge left a store that refuses: {e:#}")
            });
            for (id, want) in &oracle {
                let got = s.reconstruct(id).unwrap();
                assert_eq!(
                    &got, want,
                    "crash point {k} {variant:?}: {id} must answer identically on both sides \
                     of a merge"
                );
            }
        });
    println!("dst single-file merge: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// The native erase pipeline — tombstone flush, total merge, refold — is THREE flips, and every
/// crash point between and inside them must leave a store that opens and answers honestly:
/// survivors answer their exact bytes at every state; the erased id answers its pre-erase bytes
/// or nothing, never garbage; and once any state shows it gone, that is a committed flip's doing.
/// The directory refold's hardest window — committed swap, crashed purge — has no analogue here,
/// and this sweep is what says so.
#[test]
fn every_single_file_erase_crash_answers_honestly() {
    let root = tmp("native-erase");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("live.turndb");
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    let body_for = |i: usize| -> Vec<u8> {
        let mut b = Vec::with_capacity(800);
        let mut seed = [i as u8; 32];
        while b.len() < 800 {
            seed = blake3::hash(&seed).into();
            b.extend_from_slice(&seed);
        }
        b.truncate(800);
        b
    };

    let mut bodies = BTreeMap::new();
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
        for i in 0..6usize {
            let id = format!("e:{i}");
            let body = body_for(i);
            s.put(&id, &[Span::Piece(&body)], vec![]).unwrap();
            bodies.insert(id, body);
        }
        s.sync().unwrap();
        s.flush().unwrap();
        s.close().unwrap();
    }

    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
        s.erase_ids(&["e:2".to_string()]).unwrap();
        s.close().unwrap();
    }
    let ops = record::disarm();
    assert!(ops.len() > 10, "an erase must exercise a real op stream, got {}", ops.len());
    assert_slot_alternation("single-file erase", &ops);

    let stage = tmp("native-erase-stage");
    let checked =
        replay_recorded("single-file-erase", &base, &root, &ops, &stage, |stage, k, variant| {
            let file = stage.join("live.turndb");
            if !file.exists() {
                return;
            }
            let s = Store::open_file(&file, cfg).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: the erase left a store that refuses: {e:#}")
            });
            for (id, body) in &bodies {
                let got = s.reconstruct(id).unwrap();
                if id == "e:2" {
                    // Pre-erase bytes or gone — a committed flip decides which; drifted is the
                    // one answer no crash state may give.
                    if let Some(got) = got {
                        assert_eq!(
                            got, *body,
                            "crash point {k} {variant:?}: the erased id drifted instead of \
                             answering or vanishing"
                        );
                    }
                } else {
                    assert_eq!(
                        got.as_ref(),
                        Some(body),
                        "crash point {k} {variant:?}: {id} is a survivor and must answer exactly"
                    );
                }
            }
        });
    println!("dst single-file erase: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// The free-space punch is physical-only — no commit, no declaration, nothing referenced — so
/// its whole crash contract is: at every crash state, every record answers exactly as before,
/// and reopening never refuses. A punch that could disturb an answer would mean the free list
/// lied about a byte being free.
#[cfg(target_os = "linux")]
#[test]
fn every_single_file_punch_crash_disturbs_nothing() {
    let root = tmp("native-free-punch");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("live.turndb");
    let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
    let body_for = |i: usize| -> Vec<u8> {
        let mut b = Vec::with_capacity(9000);
        let mut seed = [i as u8; 32];
        while b.len() < 9000 {
            seed = blake3::hash(&seed).into();
            b.extend_from_slice(&seed);
        }
        b.truncate(9000);
        b
    };

    // Erase two records, then age the frees past the grace window, so the recorded punch has
    // real interiors to deallocate.
    let mut oracle = BTreeMap::new();
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
        for i in 0..5usize {
            s.put(&format!("f:{i}"), &[Span::Piece(&body_for(i))], vec![]).unwrap();
        }
        s.sync().unwrap();
        s.flush().unwrap();
        s.erase_ids(&["f:1".to_string()]).unwrap();
        for round in 0..4usize {
            s.put(&format!("age:{round}"), &[Span::Piece(&body_for(50 + round))], vec![]).unwrap();
            s.sync().unwrap();
            s.flush().unwrap();
        }
        for i in 0..5usize {
            oracle.insert(format!("f:{i}"), s.reconstruct(&format!("f:{i}")).unwrap());
        }
        for round in 0..4usize {
            let id = format!("age:{round}");
            oracle.insert(id.clone(), s.reconstruct(&id).unwrap());
        }
        s.close().unwrap();
    }

    let mut base = Fs::new(Model::Posix);
    base.seed_durable(&root);
    record::arm();
    {
        let mut s = Store::open_file(&file, cfg).unwrap();
        let stats = s.punch_free_space().unwrap();
        assert!(stats.punched_bytes > 0, "the fixture must actually punch: {stats:?}");
        s.close().unwrap();
    }
    let ops = record::disarm();
    assert!(ops.len() > 3, "a punch must exercise a real op stream, got {}", ops.len());
    assert_slot_alternation("single-file free punch", &ops);

    let stage = tmp("native-free-punch-stage");
    let checked = replay_recorded(
        "single-file-free-punch",
        &base,
        &root,
        &ops,
        &stage,
        |stage, k, variant| {
            let file = stage.join("live.turndb");
            if !file.exists() {
                return;
            }
            let s = Store::open_file(&file, cfg).unwrap_or_else(|e| {
                panic!("crash point {k} {variant:?}: the punch left a store that refuses: {e:#}")
            });
            for (id, want) in &oracle {
                let got = s.reconstruct(id).unwrap();
                assert_eq!(
                    &got, want,
                    "crash point {k} {variant:?}: {id} must be untouched by a free-space punch"
                );
            }
        },
    );
    println!("dst single-file free punch: {checked} crash states checked across {} ops", ops.len());
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&stage).ok();
}

/// The WAL sidecar beside a single-file store, by the same rule the engine uses.
fn wal_of(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}
