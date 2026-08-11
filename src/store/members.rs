//! Where a store's immutable members come from.
//!
//! A store names its parts and fold segments by store-local name and never by path — the manifest
//! validates that actively. What turns a name into bytes has always been the filesystem for a
//! writer, and [`ReadAt`] for a reader served out of a pack. This is the seam that lets a *writer*
//! make the same choice.
//!
//! It exists for one measurement. Opening a container for writing materializes every member into
//! the hot directory first, so the cost of opening a 50 GB store is copying 50 GB — even to append
//! one record. Parts and sealed fold segments are immutable, so nothing about them needs to be a
//! file; only the live segment, the manifest, and the WAL are written during a session. Resolving
//! the immutable majority to container extents makes open cost bounded by the active segment
//! rather than by the store.
//!
//! **Extents captured here stay valid for the session.** A container only ever appends, so an
//! extent handed out at open still addresses the same bytes after a checkpoint adds members. The
//! one operation that would move them, `reclaim`, already refuses a container a writer may be
//! holding — that refusal is what makes this safe, not an accident of timing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::readat::ReadAt;

/// Resolves an immutable store member — a part, a sealed fold segment, a dictionary — to bytes.
///
/// `None` from [`open`](Members::open) means *this source does not hold it*, which is a routing
/// answer and not an error: a hot directory holds what this session wrote, a container holds the
/// sealed history, and a mixed store asks both.
pub trait Members: Send + Sync {
    /// A range reader over `name`, or `None` if this source does not hold it.
    fn open(&self, name: &str) -> Result<Option<Box<dyn ReadAt>>>;

    /// Every member this source holds. The directory equivalent is `read_dir`, and callers depend
    /// on it to enumerate a namespace they cannot walk.
    fn names(&self) -> Vec<String>;

    /// Whether this source holds `name`, without opening it.
    fn contains(&self, name: &str) -> bool {
        self.names().iter().any(|n| n == name)
    }
}

/// Members as files under a directory — the reference implementation.
pub struct DirMembers {
    dir: PathBuf,
}

impl DirMembers {
    pub fn new(dir: &Path) -> DirMembers {
        DirMembers { dir: dir.to_path_buf() }
    }
}

impl Members for DirMembers {
    fn open(&self, name: &str) -> Result<Option<Box<dyn ReadAt>>> {
        let path = self.dir.join(name);
        match std::fs::File::open(&path) {
            Ok(f) => Ok(Some(Box::new(f) as Box<dyn ReadAt>)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("open member {}", path.display())),
        }
    }

    fn names(&self) -> Vec<String> {
        let mut found = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for entry in rd.flatten() {
                found.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        found.sort();
        found
    }

    fn contains(&self, name: &str) -> bool {
        self.dir.join(name).exists()
    }
}

/// Members as extents inside one container file.
///
/// Readers are captured at construction rather than resolved on demand, so the set this answers
/// for is exactly the committed state the session opened against. A member the container gains
/// later belongs to a later session's view, and serving it here would let a writer read state it
/// never committed to.
pub struct ContainerMembers {
    extents: BTreeMap<String, Arc<dyn ReadAt>>,
}

impl ContainerMembers {
    /// Capture every committed member of `container` as a reader.
    pub fn capture(container: &crate::container::Container) -> Result<ContainerMembers> {
        let mut extents: BTreeMap<String, Arc<dyn ReadAt>> = BTreeMap::new();
        for name in container.names().map(String::from).collect::<Vec<_>>() {
            let extent = container
                .extent(&name)
                .ok_or_else(|| anyhow::anyhow!("container lost member {name}"))?;
            extents.insert(name, Arc::new(extent) as Arc<dyn ReadAt>);
        }
        Ok(ContainerMembers { extents })
    }

    /// The reader for one member, shareable — the fold takes `Arc<dyn ReadAt>` directly.
    pub fn extent(&self, name: &str) -> Option<Arc<dyn ReadAt>> {
        self.extents.get(name).cloned()
    }
}

impl Members for ContainerMembers {
    fn open(&self, name: &str) -> Result<Option<Box<dyn ReadAt>>> {
        Ok(self.extents.get(name).map(|r| Box::new(Shared(r.clone())) as Box<dyn ReadAt>))
    }

    fn names(&self) -> Vec<String> {
        self.extents.keys().cloned().collect()
    }

    fn contains(&self, name: &str) -> bool {
        self.extents.contains_key(name)
    }
}

/// Wrap a shared reader so it can be handed to an API that takes ownership.
pub fn shared(reader: Arc<dyn ReadAt>) -> impl ReadAt {
    Shared(reader)
}

/// A `Box<dyn ReadAt>` view of a shared reader, so one captured extent can serve many opens.
struct Shared(Arc<dyn ReadAt>);

impl ReadAt for Shared {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> std::io::Result<()> {
        self.0.read_exact_at(buf, off)
    }

    fn len(&self) -> std::io::Result<u64> {
        self.0.len()
    }
}

/// Ask the hot directory first, then the container.
///
/// Order is the whole correctness argument. A name present in both is one the session has already
/// written a newer copy of — a rolled segment not yet checkpointed, a part rebuilt by a merge —
/// and the directory's copy is the one the manifest commits to. Asking the container first would
/// serve the state the writer superseded.
pub struct Layered {
    hot: DirMembers,
    sealed: Arc<ContainerMembers>,
}

impl Layered {
    pub fn new(hot: &Path, sealed: Arc<ContainerMembers>) -> Layered {
        Layered { hot: DirMembers::new(hot), sealed }
    }

    /// The sealed source alone, for callers that must distinguish the two namespaces — the orphan
    /// sweep, which may only ever unlink from the directory.
    pub fn sealed(&self) -> &Arc<ContainerMembers> {
        &self.sealed
    }
}

impl Members for Layered {
    fn open(&self, name: &str) -> Result<Option<Box<dyn ReadAt>>> {
        match self.hot.open(name)? {
            Some(r) => Ok(Some(r)),
            None => self.sealed.open(name),
        }
    }

    fn names(&self) -> Vec<String> {
        let mut all = self.hot.names();
        for name in self.sealed.names() {
            if !all.contains(&name) {
                all.push(name);
            }
        }
        all.sort();
        all.dedup();
        all
    }

    fn contains(&self, name: &str) -> bool {
        self.hot.contains(name) || self.sealed.contains(name)
    }
}
