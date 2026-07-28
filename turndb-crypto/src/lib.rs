//! ChaCha20-Poly1305 over turndb's [`BlockCipher`] seam, keyed per subject.
//!
//! The core defines the shape and never depends on this crate; a KMS-backed implementation is a
//! sibling and changes nothing above it. That split is the same one `turnd` makes for the network.
//!
//! # Why ChaCha20-Poly1305
//!
//! Pure Rust, so cross-compiles and wasm stay trivial and no C toolchain enters the build.
//! Constant-time by construction on every target — AES-GCM is constant-time only where AES-NI
//! exists, which is exactly the portability footgun a storage engine should not inherit. The
//! 96-bit nonce is generated randomly per block: at any plausible block count the collision
//! probability is negligible, and random beats a counter here because a counter would have to
//! survive re-folds, crashes, and key reuse across generations to stay unique.
//!
//! # What this buys, precisely
//!
//! Erasure of copies you cannot reach. Destroy a subject's key and every block sealed under it —
//! in this store, in replicas, in packs already shipped — becomes permanently unopenable. Against
//! copies you *can* reach, punch and re-fold already work and are cheaper. The residue is
//! unchanged and stated wherever erasure is claimed: piece lengths and hashes live in the parts,
//! in plaintext, until a re-fold or re-seal rebuilds them.

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use turndb::cipher::{BlockCipher, KeyId, NONCE_LEN};
use turndb::keyring::{Key, Keyring};

/// A [`BlockCipher`] over a [`Keyring`], sealing everything under one subject at a time.
///
/// The subject is a dial the writer turns (`set_subject`) — per session, per tenant, per whatever
/// the deployment erases as a unit. The measured guidance from the corpus sweeps is per-subject:
/// it costs 0.12 percentage points of dedup and about 1.43x fold bytes with a shared dictionary,
/// because dedup in trace data is trajectory-local and barely crosses subjects at all.
pub struct SubjectCipher<K: Keyring> {
    keyring: Mutex<K>,
    /// KeyId -> key material, so reads do not re-unwrap per block. Populated on demand, and a
    /// destroyed subject is REMOVED here as well as in the keyring — a cache that outlives an
    /// erasure would be a hole straight through the erasure guarantee.
    unwrapped: RwLock<HashMap<KeyId, [u8; 32]>>,
    /// The subject new blocks are sealed under.
    current: RwLock<Option<(String, KeyId)>>,
}

/// A key's stable public name: BLAKE3 of the key material, truncated. Deriving it from the key
/// rather than assigning one means two processes agree on the id without coordinating, and the id
/// leaks nothing usable — it is a hash of a secret, not a hint about it.
pub fn key_id_of(k: &Key) -> KeyId {
    let mut h = blake3::Hasher::new();
    h.update(b"turndb key id v1");
    h.update(&k.0);
    let d: [u8; 32] = h.finalize().into();
    let mut id = [0u8; 16];
    id.copy_from_slice(&d[..16]);
    KeyId(id)
}

impl<K: Keyring> SubjectCipher<K> {
    pub fn new(keyring: K) -> SubjectCipher<K> {
        SubjectCipher {
            keyring: Mutex::new(keyring),
            unwrapped: RwLock::new(HashMap::new()),
            current: RwLock::new(None),
        }
    }

    /// Seal subsequent blocks under `subject`, creating its key if absent.
    ///
    /// A writer that changes subject mid-block would put two subjects' content in one block and
    /// under one key, which is why the fold seals the open block first — see `Store::set_subject`.
    pub fn set_subject(&self, subject: &str) -> Result<KeyId> {
        let mut kr = self.keyring.lock().unwrap();
        kr.ensure(subject)?;
        let key = kr.key(subject)?.context("key vanished immediately after ensure")?;
        let id = key_id_of(&key);
        self.unwrapped.write().unwrap().insert(id, key.0);
        *self.current.write().unwrap() = Some((subject.to_string(), id));
        Ok(id)
    }

    /// DESTROY a subject's key: it leaves the keyring and the unwrapped cache together, and every
    /// block sealed under it becomes permanently unopenable — here and in every copy anywhere.
    pub fn destroy_subject(&self, subject: &str) -> Result<bool> {
        let mut kr = self.keyring.lock().unwrap();
        let id = kr.key(subject)?.map(|k| key_id_of(&k));
        let had = kr.destroy(subject)?;
        if let Some(id) = id {
            self.unwrapped.write().unwrap().remove(&id);
            let mut cur = self.current.write().unwrap();
            if cur.as_ref().is_some_and(|(_, c)| *c == id) {
                *cur = None;
            }
        }
        Ok(had)
    }

    /// Material for `id`, or `None` when the key is gone — the erasure signal.
    fn material(&self, id: KeyId) -> Result<Option<[u8; 32]>> {
        if let Some(k) = self.unwrapped.read().unwrap().get(&id) {
            return Ok(Some(*k));
        }
        // Not cached: ask the keyring for every subject whose key hashes to this id. Linear in
        // subjects and only on a cold key, which a real deployment answers with a KMS-backed
        // keyring that can look up by id directly.
        let kr = self.keyring.lock().unwrap();
        for s in kr.subjects() {
            if let Some(k) = kr.key(&s)? {
                if key_id_of(&k) == id {
                    self.unwrapped.write().unwrap().insert(id, k.0);
                    return Ok(Some(k.0));
                }
            }
        }
        Ok(None)
    }
}

impl<K: Keyring> BlockCipher for SubjectCipher<K> {
    fn current(&self) -> Result<KeyId> {
        match *self.current.read().unwrap() {
            Some((_, id)) => Ok(id),
            None => bail!("no subject is set: call set_subject before writing encrypted content"),
        }
    }

    fn select(&self, scope: &str) -> Result<KeyId> {
        self.set_subject(scope)
    }

    fn destroy_scope(&self, scope: &str) -> Result<bool> {
        self.destroy_subject(scope)
    }

    fn seal(&self, key: KeyId, plaintext: &[u8], aad: &[u8]) -> Result<([u8; NONCE_LEN], Vec<u8>)> {
        let material = self
            .material(key)?
            .with_context(|| format!("cannot seal under {key:?}: its key is gone"))?;
        let c = ChaCha20Poly1305::new((&material).into());
        let mut nonce = [0u8; NONCE_LEN];
        let mut f = std::fs::File::open("/dev/urandom").context("open /dev/urandom")?;
        std::io::Read::read_exact(&mut f, &mut nonce).context("read nonce")?;
        let ct = c
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad })
            .map_err(|_| anyhow::anyhow!("ChaCha20-Poly1305 seal failed"))?;
        Ok((nonce, ct))
    }

    fn open(
        &self,
        key: KeyId,
        nonce: &[u8; NONCE_LEN],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        // Key gone => erased. This is the ONE place that distinction is made, and everything above
        // depends on it being made honestly: `None` is erasure, `Err` is tampering.
        let Some(material) = self.material(key)? else { return Ok(None) };
        let c = ChaCha20Poly1305::new((&material).into());
        let pt = c
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad })
            .map_err(|_| {
                anyhow::anyhow!(
                    "ChaCha20-Poly1305 authentication FAILED for {key:?} — the key exists, so this \
                     is damage or tampering, not erasure"
                )
            })?;
        Ok(Some(pt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turndb::keyring::FileKeyring;

    fn ring(tag: &str) -> (std::path::PathBuf, FileKeyring) {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("turndb-crypto-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let kr = FileKeyring::open(&d.join("KEYRING"), Key([9u8; 32])).unwrap();
        (d, kr)
    }

    #[test]
    fn seals_opens_and_binds_its_associated_data() {
        let (d, kr) = ring("roundtrip");
        let c = SubjectCipher::new(kr);
        let key = c.set_subject("subject:a").unwrap();
        let aad = b"the block header";
        let pt = b"compressed block payload".repeat(40);
        let (nonce, ct) = c.seal(key, &pt, aad).unwrap();
        assert_ne!(&ct[..pt.len().min(ct.len())], &pt[..], "ciphertext must not be plaintext");
        assert_eq!(c.open(key, &nonce, &ct, aad).unwrap().unwrap(), pt);

        // AAD binding: the same ciphertext under a DIFFERENT header must not open. That is what
        // stops a block being relocated to another id or segment and still verifying.
        assert!(c.open(key, &nonce, &ct, b"a different header").is_err());
        // and a flipped ciphertext byte is damage, not erasure
        let mut bad = ct.clone();
        bad[5] ^= 1;
        assert!(c.open(key, &nonce, &bad, aad).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn destroying_a_subject_makes_its_content_unopenable_and_says_so() {
        let (d, kr) = ring("destroy");
        let c = SubjectCipher::new(kr);
        let a = c.set_subject("subject:a").unwrap();
        let (na, ca) = c.seal(a, b"alice content", b"h").unwrap();
        let b = c.set_subject("subject:b").unwrap();
        let (nb, cb) = c.seal(b, b"bob content", b"h").unwrap();
        assert_ne!(a, b, "subjects must not share a key id");

        assert!(c.destroy_subject("subject:a").unwrap());
        // ERASED — reported as absence, not as an error, so callers can say "erased" not "corrupt"
        assert!(c.open(a, &na, &ca, b"h").unwrap().is_none());
        // and nobody else is touched
        assert_eq!(c.open(b, &nb, &cb, b"h").unwrap().unwrap(), b"bob content");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn nonces_do_not_repeat() {
        let (d, kr) = ring("nonce");
        let c = SubjectCipher::new(kr);
        let k = c.set_subject("s").unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let (n, _) = c.seal(k, b"x", b"h").unwrap();
            assert!(seen.insert(n), "a repeated nonce under one key is a catastrophic failure");
        }
        std::fs::remove_dir_all(&d).ok();
    }
}
