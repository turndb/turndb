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
//! What this does not yet do is leave sealed artifacts inside the container while writing. Parts
//! and rolled segments are immutable and could be read as extents rather than copied out, which
//! would make opening independent of store size; doing that means teaching the writer's fold to
//! mix container extents with a live segment, which is surgery on the recovery path the crash
//! simulator covers. The API here is the one that survives that change — it becomes an internal
//! optimization, not a different call.

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
        let Prepared { hot, sealed } = prepare_with_members(path)?;
        let options = crate::store::StoreOptions { fold: cfg, ..Default::default() };
        let store = match sealed {
            Some(sealed) => Store::open_with_members(&hot, options, sealed),
            None => Store::open_with_options(&hot, options),
        }
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

/// Make a container ready to be written, and answer where its working state lives.
///
/// Split out from [`ContainerStore::open`] because a caller that manages its own [`Store`] — the
/// native binding's actor, for one — still has to make the same decision, and there is exactly one
/// safe answer to it. Reimplementing the adopt rule somewhere else is how a second implementation
/// gets it backwards.
///
/// Pair with [`settle`] on the way out.
pub fn prepare(container: &Path) -> Result<PathBuf> {
    Ok(prepare_with_members(container)?.hot)
}

/// A prepared working directory, and the sealed history left where it lies.
pub struct Prepared {
    /// Where this session's mutable state lives.
    pub hot: PathBuf,
    /// Immutable members still inside the container, for [`Store::open_with_members`]. `None` when
    /// there is no container behind this session yet.
    pub sealed: Option<Arc<ContainerMembers>>,
}

/// [`prepare`], also answering where the members it did not copy can be read.
///
/// Parts are immutable once committed, so copying them out to append one record is work with no
/// product. They stay in the container and the writer reads them as extents; only state a session
/// mutates — the manifest, the WAL, the live fold segment — has to be a file.
pub fn prepare_with_members(container: &Path) -> Result<Prepared> {
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

    let mut buf = vec![0u8; 1 << 20];
    for name in container.names().map(String::from).collect::<Vec<_>>() {
        // Parts are immutable once committed and the writer reads them as extents, so copying one
        // out produces a second identical byte range and nothing else. Skipping them is what makes
        // opening cost the session's own state rather than the store's whole history.
        if is_part(&name) {
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
