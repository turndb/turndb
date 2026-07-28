//! Keys, and the erasure they buy — the envelope model behind crypto-erasure.
//!
//! # Why an envelope rather than derivation
//!
//! A derived key (`KDF(master, subject)`) cannot be destroyed: anyone holding the master
//! re-derives it, so "erasure by key destruction" would be a claim with a trapdoor. So a subject's
//! key is **random**, stored **wrapped** by a key-encryption key, and erasing a subject means
//! deleting its wrapped bytes. What survives is ciphertext no one can open — which is the whole
//! mechanism, and the reason it is worth the machinery.
//!
//! # The granularity, decided by measurement
//!
//! `.scratch/cryptoerase` swept key grain against dedup on 40k real records / 204 subjects:
//! per-session keys reached 97.31% dedup where a single store-wide key reached 97.43% — **0.12
//! percentage points**, because dedup is trajectory-local and cross-subject sharing is nearly
//! absent. Blocks must then be packed per subject (a block is the unit of encryption), which the
//! same sweep priced at 1.53x fold bytes, falling to **1.43x with one shared trained dictionary**.
//! That is the cost of erasure-by-key-destruction, and it is affordable exactly because the
//! trajectory-locality that makes this engine work also makes per-subject keys nearly free.
//!
//! Per-RECORD keys, for contrast, cost 77x. The sweep is the argument against them.
//!
//! # What this module deliberately does NOT do
//!
//! It defines the key hierarchy, the wrapping format, and destruction — the parts that are
//! policy. It does not implement AEAD: encryption belongs to the fold's block path, where the
//! `flags` reject-forward lever guards the format change, and it lands with that work. This
//! module is what erasure destroys.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A subject: whatever the deployment erases as a unit — a person, a tenant, a session. The store
/// learns it from an attribute (`turndb.subject` by convention) and never interprets it.
pub type SubjectId = String;

/// 32 bytes of key material. Zeroed on drop — best-effort, because Rust makes no promise about
/// copies the allocator or the optimiser already made; the durable guarantee is that the WRAPPED
/// bytes are gone from disk, not that RAM was scrubbed.
pub struct Key(pub [u8; 32]);

impl Drop for Key {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // volatile so the write is not optimised away as dead
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl Key {
    /// A fresh random key, from the OS. Deliberately not derived: see the module note.
    pub fn generate() -> Result<Key> {
        let mut k = [0u8; 32];
        let mut f = std::fs::File::open("/dev/urandom").context("open /dev/urandom")?;
        std::io::Read::read_exact(&mut f, &mut k).context("read key material")?;
        Ok(Key(k))
    }
}

/// A wrapped subject key as it sits on disk. The wrapping here is XOR with a KEK-derived stream —
/// a PLACEHOLDER whose shape is right and whose cryptography is not, marked so nobody mistakes
/// it: real wrapping is AES-KW or ChaCha20-Poly1305 and lands with the AEAD work. What is already
/// correct, and what the erasure story actually rests on, is that destroying these bytes destroys
/// access.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedKey {
    pub subject: SubjectId,
    /// Which KEK generation wrapped it — rotation without rewrapping the world.
    pub kek_gen: u32,
    pub bytes: Vec<u8>,
}

/// The keyring: subject → wrapped key, plus destruction. File-backed by default; the trait is the
/// seam a KMS implementation slots into without the store noticing.
pub trait Keyring: Send + Sync {
    /// The subject's key, unwrapped, or `None` when it was destroyed (or never existed) — which
    /// is exactly what an erased subject looks like, and why the caller must handle it as data
    /// rather than as an error.
    fn key(&self, subject: &str) -> Result<Option<Key>>;
    /// Create and store a key for `subject` if absent; return whether one was created.
    fn ensure(&mut self, subject: &str) -> Result<bool>;
    /// DESTROY the subject's key. Returns whether one was there to destroy. After this, every
    /// block encrypted under it is permanently unopenable by this keyring.
    fn destroy(&mut self, subject: &str) -> Result<bool>;
    /// Subjects with live keys.
    fn subjects(&self) -> Vec<SubjectId>;
}

/// A keyring in a file beside the store — the default, and the one an operator can back up,
/// escrow, or (deliberately) lose.
///
/// The file is rewritten wholly on every change: it is small, and a torn partial rewrite of a
/// keyring is a worse failure than any write amplification it avoids. Written tmp + fsync +
/// rename + fsync-dir, the same commit discipline as the manifest.
pub struct FileKeyring {
    path: PathBuf,
    kek: Key,
    kek_gen: u32,
    keys: BTreeMap<SubjectId, WrappedKey>,
}

impl FileKeyring {
    /// Open (or create) the keyring at `path`, wrapped by `kek`.
    pub fn open(path: &Path, kek: Key) -> Result<FileKeyring> {
        let keys = match std::fs::read(path) {
            Ok(b) => decode(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e).context(format!("read keyring {}", path.display())),
        };
        Ok(FileKeyring { path: path.to_path_buf(), kek, kek_gen: 1, keys })
    }

    fn persist(&self) -> Result<()> {
        let tmp = self.path.with_extension("keyring.tmp");
        let f = crate::vfs::create(&tmp)?;
        crate::vfs::write_all_at(&f, &tmp, &encode(&self.keys), 0)?;
        crate::vfs::sync_file(&f, &tmp)?;
        drop(f);
        crate::vfs::rename(&tmp, &self.path)?;
        if let Some(parent) = self.path.parent() {
            crate::vfs::sync_dir(parent)?;
        }
        Ok(())
    }

    /// PLACEHOLDER wrapping — see [`WrappedKey`].
    fn wrap(&self, k: &Key) -> Vec<u8> {
        let mut out = k.0.to_vec();
        let mut stream = blake3::Hasher::new();
        stream.update(&self.kek.0);
        stream.update(&self.kek_gen.to_le_bytes());
        let pad: [u8; 32] = stream.finalize().into();
        for (o, p) in out.iter_mut().zip(pad) {
            *o ^= p;
        }
        out
    }

    fn unwrap_key(&self, w: &WrappedKey) -> Result<Key> {
        if w.bytes.len() != 32 {
            bail!("wrapped key for {} is {} bytes, not 32", w.subject, w.bytes.len());
        }
        let mut stream = blake3::Hasher::new();
        stream.update(&self.kek.0);
        stream.update(&w.kek_gen.to_le_bytes());
        let pad: [u8; 32] = stream.finalize().into();
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = w.bytes[i] ^ pad[i];
        }
        Ok(Key(k))
    }
}

impl Keyring for FileKeyring {
    fn key(&self, subject: &str) -> Result<Option<Key>> {
        match self.keys.get(subject) {
            Some(w) => Ok(Some(self.unwrap_key(w)?)),
            None => Ok(None),
        }
    }

    fn ensure(&mut self, subject: &str) -> Result<bool> {
        if self.keys.contains_key(subject) {
            return Ok(false);
        }
        let k = Key::generate()?;
        let w = WrappedKey { subject: subject.to_string(), kek_gen: self.kek_gen, bytes: self.wrap(&k) };
        self.keys.insert(subject.to_string(), w);
        self.persist()?;
        Ok(true)
    }

    fn destroy(&mut self, subject: &str) -> Result<bool> {
        let had = self.keys.remove(subject).is_some();
        if had {
            self.persist()?;
        }
        Ok(had)
    }

    fn subjects(&self) -> Vec<SubjectId> {
        self.keys.keys().cloned().collect()
    }
}

/// `subject \0 kek_gen(4) \0 len(4) bytes` per entry, length-prefixed count first. Deliberately
/// hand-rolled: the keyring is durable state, and its encoding should not be a dependency's
/// choice any more than the fold's is.
fn encode(keys: &BTreeMap<SubjectId, WrappedKey>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"TURNKEYS");
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for (s, w) in keys {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(&w.kek_gen.to_le_bytes());
        out.extend_from_slice(&(w.bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&w.bytes);
    }
    let x = blake3::hash(&out);
    out.extend_from_slice(&x.as_bytes()[0..4]);
    out
}

fn decode(b: &[u8]) -> Result<BTreeMap<SubjectId, WrappedKey>> {
    if b.len() < 16 || &b[0..8] != b"TURNKEYS" {
        bail!("not a turndb keyring");
    }
    let body = &b[..b.len() - 4];
    if blake3::hash(body).as_bytes()[0..4] != b[b.len() - 4..] {
        bail!("keyring checksum mismatch — refusing to open a damaged keyring");
    }
    let n = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
    let mut at = 12usize;
    let mut out = BTreeMap::new();
    let take = |at: &mut usize, n: usize| -> Result<&[u8]> {
        if n > body.len() - *at {
            bail!("keyring entry runs past the file");
        }
        let s = &body[*at..*at + n];
        *at += n;
        Ok(s)
    };
    for _ in 0..n {
        let sl = u32::from_le_bytes(take(&mut at, 4)?.try_into().unwrap()) as usize;
        let subject = String::from_utf8(take(&mut at, sl)?.to_vec())?;
        let kek_gen = u32::from_le_bytes(take(&mut at, 4)?.try_into().unwrap());
        let bl = u32::from_le_bytes(take(&mut at, 4)?.try_into().unwrap()) as usize;
        let bytes = take(&mut at, bl)?.to_vec();
        out.insert(subject.clone(), WrappedKey { subject, kek_gen, bytes });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("turndb-keyring-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn keys_persist_unwrap_and_destroy() {
        let d = tmpdir("basic");
        let p = d.join("KEYRING");
        let kek = Key([7u8; 32]);
        let mut kr = FileKeyring::open(&p, kek).unwrap();
        assert!(kr.ensure("subject:alice").unwrap());
        assert!(!kr.ensure("subject:alice").unwrap(), "ensure is idempotent");
        kr.ensure("subject:bob").unwrap();
        let alice = kr.key("subject:alice").unwrap().unwrap().0;
        assert_ne!(alice, [0u8; 32]);
        assert_ne!(alice, kr.key("subject:bob").unwrap().unwrap().0, "subjects must not share keys");

        // reopen: the same key comes back, which is what makes stored data readable tomorrow
        drop(kr);
        let mut kr = FileKeyring::open(&p, Key([7u8; 32])).unwrap();
        assert_eq!(kr.key("subject:alice").unwrap().unwrap().0, alice);

        // DESTRUCTION: the key is gone from memory and from disk, and absence is data, not error
        assert!(kr.destroy("subject:alice").unwrap());
        assert!(!kr.destroy("subject:alice").unwrap(), "destroying twice is not an error");
        assert!(kr.key("subject:alice").unwrap().is_none());
        drop(kr);
        let kr = FileKeyring::open(&p, Key([7u8; 32])).unwrap();
        assert!(kr.key("subject:alice").unwrap().is_none(), "destruction must survive reopen");
        assert!(kr.key("subject:bob").unwrap().is_some(), "and must not touch anyone else");
        assert!(!std::fs::read(&p).unwrap().windows(32).any(|w| w == alice),
            "the destroyed key's material must not remain in the keyring file");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_damaged_keyring_refuses_rather_than_losing_subjects_quietly() {
        let d = tmpdir("damaged");
        let p = d.join("KEYRING");
        let mut kr = FileKeyring::open(&p, Key([3u8; 32])).unwrap();
        kr.ensure("s1").unwrap();
        kr.ensure("s2").unwrap();
        drop(kr);

        let mut b = std::fs::read(&p).unwrap();
        let at = b.len() / 2;
        b[at] ^= 0xFF;
        std::fs::write(&p, &b).unwrap();
        assert!(
            FileKeyring::open(&p, Key([3u8; 32])).is_err(),
            "a damaged keyring must refuse — silently losing a subject's key is silently erasing them"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_wrong_kek_does_not_yield_the_right_key() {
        let d = tmpdir("kek");
        let p = d.join("KEYRING");
        let mut kr = FileKeyring::open(&p, Key([1u8; 32])).unwrap();
        kr.ensure("s").unwrap();
        let right = kr.key("s").unwrap().unwrap().0;
        drop(kr);
        let kr = FileKeyring::open(&p, Key([2u8; 32])).unwrap();
        assert_ne!(kr.key("s").unwrap().unwrap().0, right, "a different KEK must not unwrap to the same key");
        std::fs::remove_dir_all(&d).ok();
    }
}
