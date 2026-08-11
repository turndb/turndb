//! Writing to a store you hold as one file.
//!
//! A container is a complete store in a single file, but the write path is directory-shaped:
//! append semantics, fsync, and rename atomicity are properties a directory has and a byte range
//! inside a file does not. So a writable container keeps the shape SQLite settled on — one file at
//! rest, working state beside it while open:
//!
//! ```text
//! mystore.turndb        the committed store: MANIFEST, parts, fold segments
//! mystore.turndb-hot/   working state while a writer holds it
//! ```
//!
//! [`ContainerStore::open`] materializes the hot directory from the container, hands out an
//! ordinary [`Store`] over it, and [`checkpoint`](ContainerStore::checkpoint) folds the result
//! back in. A clean [`close`](ContainerStore::close) checkpoints and removes the hot directory, so
//! what remains is the file you started with. A crash leaves it behind instead, and the next open
//! resumes from it rather than from the container: the hot directory is the newer state, and
//! materializing over it would discard acknowledged writes.
//!
//! **The hot directory is where the writer lock lives.** Exclusion is the engine's ordinary
//! `flock` on the fold, unchanged — a container has no lock of its own, and two writers holding
//! one `.turndb` are excluded because they contend for the same hot directory.
//!
//! **Sealed artifacts stay in the container while a writer works.** Parts and sealed fold segments
//! are immutable, so they are read as extents rather than copied out, and opening costs the state a
//! session can actually change rather than the whole store's history. What materializes is the
//! manifest, the dictionaries, the sidecars, and fold segments from the committed tail's segment
//! upward — the tail's own because recovery TRUNCATES it, and any above it because recovery
//! UNLINKS those, and neither is something that can be done to a member of a container.
//!
//! The remaining copy is therefore bounded by `seg_max` rather than by the store: opening a 50 GB
//! container costs one segment, not fifty gigabytes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::members::ContainerMembers;
use super::{checkpoint_into_container, CheckpointStats, Store};
use crate::container::{Container, HOT_SUFFIX};
use crate::fold::FoldCfg;

/// A writable store held as one file.
pub struct ContainerStore {
    store: Option<Store>,
    container: PathBuf,
    hot: PathBuf,
}

impl ContainerStore {
    /// Open a `.turndb` for writing, creating it if it does not exist.
    ///
    /// Resuming an interrupted session is the default, not a repair: a hot directory that outlived
    /// its writer holds writes the container has not been told about, so it is adopted as-is.
    pub fn open(path: &Path, cfg: FoldCfg) -> Result<ContainerStore> {
        let prepared = prepare(path)?;
        let hot = prepared.hot.clone();
        let store =
            prepared
                .open(crate::store::StoreOptions { fold: cfg, ..Default::default() })
                .with_context(|| format!("open the working directory for {}", path.display()))?;
        Ok(ContainerStore { store: Some(store), container: path.to_path_buf(), hot })
    }

    /// The engine, in full. Every ordinary store operation applies.
    pub fn store(&mut self) -> &mut Store {
        self.store.as_mut().expect("a ContainerStore holds its store until close")
    }

    /// Where the working state lives while this handle is open.
    pub fn hot_directory(&self) -> &Path {
        &self.hot
    }

    /// Settle outstanding writes and fold them into the container.
    ///
    /// Durable before it returns: the sync and flush happen first, so the container is only ever
    /// told about state the directory has already committed.
    pub fn checkpoint(&mut self) -> Result<CheckpointStats> {
        let store = self.store.as_mut().expect("open");
        store.sync()?;
        store.flush()?;
        checkpoint_into_container(&self.hot, &self.container)
    }

    /// Checkpoint, release the writer, and remove the working directory.
    ///
    /// After this the container is the only artifact — the promise the single-file shape is for.
    /// The directory is removed last: if anything before it fails, the working state is still
    /// there to open again.
    pub fn close(mut self) -> Result<CheckpointStats> {
        let store = self.store.as_mut().expect("open");
        store.sync()?;
        store.flush()?;
        // Drop the store first so the fold's writer lock is released before the tree goes.
        self.store = None;
        settle(&self.container)
    }
}

/// A prepared working directory, and the sealed history left where it lies.
///
/// Both halves or neither. This used to be a bare `PathBuf`, and it must not be one again: the
/// directory alone is no longer a complete store, so a caller that opens a plain [`Store`] over it
/// gets one missing every member that stayed in the container. That is not a hypothetical — it is
/// what the native binding did the moment parts stopped being copied, and the type is what makes
/// the second half impossible to drop.
pub struct Prepared {
    /// Where this session's mutable state lives.
    pub hot: PathBuf,
    /// Immutable members still inside the container, for [`Store::open_with_members`]. `None` when
    /// there is no container behind this session yet, and only then.
    pub sealed: Option<Arc<ContainerMembers>>,
}

impl Prepared {
    /// Open the writer this prepared, routing sealed members to wherever they actually are.
    pub fn open(self, options: crate::store::StoreOptions) -> Result<Store> {
        match self.sealed {
            Some(sealed) => Store::open_with_members(&self.hot, options, sealed),
            None => Store::open_with_options(&self.hot, options),
        }
    }
}

/// Make a container ready to be written, and answer where its state now lives — both halves of it.
///
/// Split out from [`ContainerStore::open`] because a caller that manages its own [`Store`] — the
/// native binding's actor, for one — still has to make the same decision, and there is exactly one
/// safe answer to it. Reimplementing the adopt rule somewhere else is how a second implementation
/// gets it backwards.
///
/// Feed the result to [`Prepared::open`] rather than opening a [`Store`] over the path yourself.
/// Pair with [`settle`] on the way out.
pub fn prepare(container: &Path) -> Result<Prepared> {
    let hot = hot_path(container);
    let exists = container.exists();

    if hot.exists() {
        // Adopt it. Its state is at least as new as the container's, and re-materializing would
        // overwrite acknowledged writes with an older committed snapshot.
        if exists {
            // Refuse the one case that is not a resume: a working directory beside a container it
            // did not come from is a name collision, and guessing which to keep loses data.
            let open = Container::open(container).with_context(|| {
                format!("{} has a hot directory but is not a container", container.display())
            })?;
            // An adopted directory may hold its own copy of a member — an earlier session that
            // materialized everything, or one interrupted mid-roll. `Store` prefers the directory
            // for any name present in both, so offering the container's copy alongside is safe.
            return Ok(Prepared { sealed: Some(Arc::new(ContainerMembers::capture(&open)?)), hot });
        }
    } else if exists {
        let open = Container::open(container)?;
        materialize(&open, &hot)?;
        return Ok(Prepared { sealed: Some(Arc::new(ContainerMembers::capture(&open)?)), hot });
    } else {
        crate::vfs::mkdir_all(&hot)?;
    }
    Ok(Prepared { hot, sealed: None })
}

/// Fold a prepared working directory back into its container and remove it.
///
/// The caller must have settled its own writes first — this ingests what the directory has
/// committed, and cannot see what a writer has not yet flushed. The directory goes last: if the
/// checkpoint fails, the working state is still there to open again.
pub fn settle(container: &Path) -> Result<CheckpointStats> {
    let hot = hot_path(container);
    let stats = checkpoint_into_container(&hot, container)?;
    crate::vfs::remove_tree(&hot)
        .with_context(|| format!("remove the working directory {}", hot.display()))?;
    Ok(stats)
}

/// Whether a member name is a part.
///
/// The same shape the orphan sweep matches on, and deliberately not a looser test: a fold segment
/// or a sidecar that fell through here would be left out of a working directory that needs it.
fn is_part(name: &str) -> bool {
    name.starts_with("part-") && name.ends_with(".part")
}

/// The lowest fold segment a working directory must hold as a real file.
///
/// Everything below it is sealed: the committed tail is strictly beyond it, so no truncation can
/// ever apply. Everything from it up must be materialized — the segment holding the tail because
/// recovery truncates it, and any segment above it because recovery UNLINKS those, which it cannot
/// do to a member of a container.
///
/// A container with no readable manifest yields 0, which materializes everything. That is the
/// conservative answer and the one that matches what this did before any of it was skipped.
fn first_live_segment(container: &Container) -> u32 {
    let Ok(bytes) = container.read_file_bounded("MANIFEST", crate::store::MAX_MANIFEST_BYTES)
    else {
        return 0;
    };
    match super::Manifest::parse(&bytes) {
        Ok(manifest) => manifest.fold_tail().map(|tail| tail.seg).unwrap_or(0),
        Err(_) => 0,
    }
}

/// Whether `name` is a fold segment sealed below `first_live`.
fn is_sealed_segment(name: &str, first_live: u32) -> bool {
    let Some((_, file)) = name.rsplit_once('/') else {
        return false;
    };
    match crate::fold::segment::parse_seg_name(file) {
        Some(n) => n < first_live,
        None => false,
    }
}

/// The working directory that belongs to a container.
pub fn hot_path(container: &Path) -> PathBuf {
    let mut name = container.as_os_str().to_os_string();
    name.push(HOT_SUFFIX);
    PathBuf::from(name)
}

/// Write every container member out as a real file, rebuilding the store directory it came from.
fn materialize(container: &Container, hot: &Path) -> Result<()> {
    // Stage under a temporary name and rename into place, so an interrupted materialization cannot
    // leave a half-populated directory that the next open would adopt as a resume.
    let staging = hot.with_extension("materializing");
    let _ = crate::vfs::remove_tree(&staging);
    crate::vfs::mkdir_all(&staging)?;

    // Streamed in a fixed window rather than read whole. A part is the largest thing a store
    // holds and has no ceiling of its own; reading one into a Vec to write it straight back out
    // would be an unbounded allocation on the most-travelled path in this module, in an engine
    // whose entire read side is admission-bounded precisely so that cannot happen.
    // Every directory the members land in, so their NAMES can be made durable before the tree is
    // published. Content fsyncs are not enough: a dirent is durable only at its parent's fsync,
    // and publishing a directory whose name survives while its entries do not produces exactly the
    // failure this guards — a working directory that looks complete, is adopted on the next open,
    // and is missing files. `pack::extract_into` has always done this; this did not.
    let mut dirs: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    dirs.insert(staging.clone());

    let first_live = first_live_segment(container);
    let mut buf = vec![0u8; 1 << 20];
    for name in container.names().map(String::from).collect::<Vec<_>>() {
        // Parts and sealed segments are immutable once committed and the writer reads them as
        // extents, so copying one out produces a second identical byte range and nothing else.
        // Skipping them is what makes opening cost the session's own state rather than the store's
        // whole history. Sidecars are not skipped: they are small next to the segments they
        // describe, and leaving them on the directory path keeps the fold's block-directory
        // rebuild identical for both kinds of segment.
        if is_part(&name) || is_sealed_segment(&name, first_live) {
            continue;
        }
        let source = container
            .extent(&name)
            .ok_or_else(|| anyhow::anyhow!("container lost member {name}"))?;
        let len = crate::readat::ReadAt::len(&source)?;
        let dst = staging.join(&name);
        if let Some(parent) = dst.parent() {
            crate::vfs::mkdir_all(parent)?;
            let mut ancestor = Some(parent);
            while let Some(d) = ancestor {
                if !d.starts_with(&staging) {
                    break;
                }
                dirs.insert(d.to_path_buf());
                if d == staging {
                    break;
                }
                ancestor = d.parent();
            }
        }
        let f = crate::vfs::create(&dst)?;
        let mut at = 0u64;
        while at < len {
            let take = buf.len().min((len - at) as usize);
            crate::readat::ReadAt::read_exact_at(&source, &mut buf[..take], at)?;
            crate::vfs::write_all_at(&f, &dst, &buf[..take], at)?;
            at += take as u64;
        }
        crate::vfs::sync_file(&f, &dst)?;
    }
    if !staging.join("MANIFEST").exists() {
        bail!("container holds no MANIFEST, so it names no store to open");
    }
    // Children before parents: a parent's fsync does not promote a child directory's own entries.
    let mut ordered: Vec<&PathBuf> = dirs.iter().collect();
    ordered.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in ordered {
        crate::vfs::sync_dir(d)?;
    }
    crate::vfs::rename_noreplace(&staging, hot)
        .with_context(|| format!("publish the working directory {}", hot.display()))?;
    if let Some(parent) = hot.parent() {
        let _ = crate::vfs::sync_dir(parent);
    }
    Ok(())
}
