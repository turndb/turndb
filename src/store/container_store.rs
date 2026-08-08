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

use anyhow::{bail, Context, Result};

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
        let hot = hot_path(path);
        let container_exists = path.exists();

        if hot.exists() {
            // Adopt it. Its state is at least as new as the container's, and re-materializing
            // would overwrite acknowledged writes with an older committed snapshot.
            if container_exists {
                // Refuse the one case that is not a resume: a hot directory beside a container it
                // did not come from is a name collision, and guessing which to keep loses data.
                Container::open(path).with_context(|| {
                    format!("{} has a hot directory but is not a container", path.display())
                })?;
            }
        } else if container_exists {
            let container = Container::open(path)?;
            materialize(&container, &hot)?;
        } else {
            crate::vfs::mkdir_all(&hot)?;
        }

        let store = Store::open(&hot, cfg)
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
        let stats = self.checkpoint()?;
        // Drop the store first so the fold's writer lock is released before the tree goes.
        self.store = None;
        crate::vfs::remove_tree(&self.hot)
            .with_context(|| format!("remove the working directory {}", self.hot.display()))?;
        Ok(stats)
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
    let mut buf = vec![0u8; 1 << 20];
    for name in container.names().map(String::from).collect::<Vec<_>>() {
        let source = container
            .extent(&name)
            .ok_or_else(|| anyhow::anyhow!("container lost member {name}"))?;
        let len = crate::readat::ReadAt::len(&source)?;
        let dst = staging.join(&name);
        if let Some(parent) = dst.parent() {
            crate::vfs::mkdir_all(parent)?;
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
    crate::vfs::rename_noreplace(&staging, hot)
        .with_context(|| format!("publish the working directory {}", hot.display()))?;
    if let Some(parent) = hot.parent() {
        let _ = crate::vfs::sync_dir(parent);
    }
    Ok(())
}
