//! The end-to-end claim: content sealed into a real fold becomes permanently unreadable when its
//! key is destroyed — and the fold says ERASED, not "corrupt".

use std::sync::Arc;
use turndb::fold::{Fold, FoldCfg};
use turndb::keyring::{FileKeyring, Key};
use turndb_crypto::SubjectCipher;

fn tmp(tag: &str) -> std::path::PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("turndb-enc-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn an_encrypted_fold_round_trips_and_destroying_the_key_erases_it() {
    let d = tmp("roundtrip");
    let kr = FileKeyring::open(&d.join("KEYRING"), Key([5u8; 32])).unwrap();
    let cipher = Arc::new(SubjectCipher::new(kr));
    cipher.set_subject("subject:alice").unwrap();

    let cfg = FoldCfg { block_target: 8 * 1024, ..Default::default() };
    let bodies: Vec<Vec<u8>> = (0..12)
        .map(|i| format!("{{\"turn\":{i},\"content\":\"{}\"}}", "m".repeat(900 + i * 13)).into_bytes())
        .collect();

    let locs: Vec<_> = {
        let mut f = Fold::open(&d.join("fold"), cfg).unwrap().with_cipher(cipher.clone()).unwrap();
        let locs: Vec<_> = bodies.iter().map(|b| f.put(b).unwrap()).collect();
        f.sync().unwrap();
        // readable through the writer, exactly as a plaintext fold is
        for (p, want) in locs.iter().zip(&bodies) {
            assert_eq!(&f.read_verified(p.loc, p.hash).unwrap(), want);
        }
        locs
    };

    // The bytes on disk are CIPHERTEXT: the plaintext must not appear in the segment.
    let seg = std::fs::read(d.join("fold").join("seg-00000000.fold")).unwrap();
    let needle = &bodies[0][..40];
    assert!(
        !seg.windows(needle.len()).any(|w| w == needle),
        "plaintext must not be present in an encrypted segment"
    );

    // A reader WITH the cipher reads everything back byte-exact across a reopen.
    {
        let f = Fold::open_read(&d.join("fold"), cfg).unwrap();
        let kr = FileKeyring::open(&d.join("KEYRING"), Key([5u8; 32])).unwrap();
        let c2 = Arc::new(SubjectCipher::new(kr));
        let f = f.with_cipher_readonly(c2);
        for (p, want) in locs.iter().zip(&bodies) {
            assert_eq!(&f.read_verified(p.loc, p.hash).unwrap(), want, "encrypted read drifted");
        }
    }

    // A reader WITHOUT keys gets a refusal that names the problem — not a decode error.
    {
        let f = Fold::open_read(&d.join("fold"), cfg).unwrap();
        let err = format!("{:#}", f.read_verified(locs[0].loc, locs[0].hash).unwrap_err());
        assert!(err.contains("no cipher") || err.contains("unreadable without keys"), "got: {err}");
    }

    // DESTROY the key: the same reader, now with a cipher whose subject is gone, must report
    // ERASURE by name. This is the whole point of the exercise.
    {
        let kr = FileKeyring::open(&d.join("KEYRING"), Key([5u8; 32])).unwrap();
        let c3 = Arc::new(SubjectCipher::new(kr));
        assert!(c3.destroy_subject("subject:alice").unwrap());
        let f = Fold::open_read(&d.join("fold"), cfg).unwrap().with_cipher_readonly(c3);
        let err = format!("{:#}", f.read_verified(locs[0].loc, locs[0].hash).unwrap_err());
        assert!(err.contains("ERASED"), "a destroyed key must read as ERASED, got: {err}");
    }
    std::fs::remove_dir_all(&d).ok();
}
