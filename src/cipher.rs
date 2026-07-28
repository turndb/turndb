//! The encryption seam: what the core knows about ciphers, which is as little as possible.
//!
//! The core defines the shape and never implements it. `turndb-crypto` provides an
//! implementation; a KMS-backed one is another impl and nothing here changes. That split is the
//! same one `turnd` follows for the network, and for the same reason: the substrate's dependency
//! surface is an identity, not an accident.
//!
//! # What encryption is FOR here
//!
//! Not confidentiality-at-rest for its own sake — full-disk encryption does that without a format
//! change. It is for **erasure of copies you cannot reach**: replicas, backups, packs already
//! shipped to a counterparty. Destroy the key and every copy everywhere becomes unopenable, which
//! is the only mechanism that reaches them. That is why [`BlockCipher::open`] returns `None` for a
//! destroyed key instead of an error: an erased subject is a normal, expected, *reportable* state,
//! and code that treats it as a failure will eventually treat it as corruption.
//!
//! # Ordering, and the two leaks it does not fix
//!
//! Compress THEN encrypt: ciphertext does not compress, and the measured cost of getting this
//! backwards is a fold that barely shrinks at all. What survives key destruction, and must be said
//! plainly wherever erasure is claimed:
//!
//! * **piece lengths**, in every part's `pdict.loc` — a required section, plaintext by design.
//!   Measured on a real corpus: 59.9% of pieces are under 1 KiB, so the length profile is a real
//!   fingerprint, not a theoretical one. Only a re-fold or re-seal rebuilds parts and removes it.
//! * **piece hashes**, in `pdict.hash` — unkeyed BLAKE3, so a guessed plaintext is confirmable.
//!
//! Encryption erases *content*. Metadata residue is a parts-plane problem with a parts-plane
//! answer, and conflating the two would be the kind of overclaim this project refuses to make.

use anyhow::Result;

/// Names a key without being one. Sixteen bytes, stable, derived from the key material by the
/// implementation — long enough that a block's key is unambiguous, and it rides in each encrypted
/// frame so a reader knows what to ask the keyring for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct KeyId(pub [u8; 16]);

impl KeyId {
    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "key:{:02x}{:02x}{:02x}{:02x}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

/// Bytes an encrypted frame carries beyond its ciphertext: the key id, the nonce, and the tag.
pub const KEY_ID_LEN: usize = 16;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
/// Total per-block framing overhead — 44 bytes on a multi-megabyte block, which is nothing, and
/// the reason blocks rather than pieces are the unit of encryption.
pub const AEAD_OVERHEAD: usize = KEY_ID_LEN + NONCE_LEN + TAG_LEN;

/// Authenticated encryption over whole compressed blocks.
///
/// Implementations must be safe for concurrent use: reads happen on every scan partition.
pub trait BlockCipher: Send + Sync {
    /// The key new content should be written under. Called once per sealed block, so an
    /// implementation is free to make this per-subject, per-session, or per-anything — the fold
    /// does not know or care what the grain means.
    fn current(&self) -> Result<KeyId>;

    /// Encrypt `plaintext`, authenticating `aad` alongside it. Returns `(nonce, ciphertext||tag)`.
    ///
    /// `aad` is the block's frame header. Binding it means ciphertext cannot be moved to another
    /// frame, another block id, or another segment and still open — an attacker who can rewrite
    /// the fold cannot reshuffle it into something that verifies.
    fn seal(&self, key: KeyId, plaintext: &[u8], aad: &[u8]) -> Result<([u8; NONCE_LEN], Vec<u8>)>;

    /// Decrypt, or report that the key is gone.
    ///
    /// * `Ok(Some(bytes))` — opened and authenticated.
    /// * `Ok(None)` — **the key was destroyed**: this content is permanently unreadable, by
    ///   design, and every layer above must say "erased" rather than "corrupt".
    /// * `Err(..)` — the key exists and the ciphertext did not authenticate. That is tampering or
    ///   damage, and it is not the same thing at all.
    fn open(
        &self,
        key: KeyId,
        nonce: &[u8; NONCE_LEN],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Option<Vec<u8>>>;
}
