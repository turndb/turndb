//! Crypto-erasure through the STORE — the capability the whole item exists for.
//!
//! Destroying a scope's key makes its content unopenable everywhere, including in copies this
//! process cannot reach; the re-fold then removes the local bytes and the metadata the key never
//! covered. This test asserts both halves, and asserts that neither touches anyone else.

use std::sync::Arc;
use turndb::fold::FoldCfg;
use turndb::keyring::{FileKeyring, Key};
use turndb::store::{Span, Store};
use turndb::AttrValue;
use turndb_crypto::SubjectCipher;

fn tmp(tag: &str) -> std::path::PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("turndb-scope-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cipher(d: &std::path::Path) -> Arc<SubjectCipher<FileKeyring>> {
    let kr = FileKeyring::open(&d.join("KEYRING"), Key([11u8; 32])).unwrap();
    Arc::new(SubjectCipher::new(kr))
}

/// Records for one subject, each body unique so nothing dedups across subjects by accident.
fn write_subject(s: &mut Store, subject: &str, n: usize) -> Vec<(String, Vec<u8>)> {
    s.set_scope(subject).unwrap();
    let mut out = Vec::new();
    for i in 0..n {
        let id = format!("{subject}:rec{i}");
        let body =
            format!("{{\"subject\":\"{subject}\",\"i\":{i},\"pad\":\"{}\"}}", "p".repeat(500 + i * 7));
        s.put(
            &id,
            &[Span::Piece(body.as_bytes())],
            vec![("turndb.subject".into(), AttrValue::Str(subject.into()))],
        )
        .unwrap();
        out.push((id, body.into_bytes()));
    }
    out
}

#[test]
fn destroying_a_scope_key_erases_only_that_scope_and_leaves_the_rest_byte_exact() {
    let d = tmp("erase");
    let dir = d.join("store");
    let c = cipher(&d);

    let (alice, bob) = {
        let mut s = Store::open_encrypted(&dir, FoldCfg { block_target: 8 * 1024, ..Default::default() }, c.clone())
            .unwrap();
        let alice = write_subject(&mut s, "subject:alice", 6);
        let bob = write_subject(&mut s, "subject:bob", 6);
        s.sync().unwrap();
        s.flush().unwrap();
        // both readable while both keys live
        for (id, body) in alice.iter().chain(bob.iter()) {
            assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "{id} unreadable before erasure");
        }
        (alice, bob)
    };

    // ERASE alice: key destroyed, then records removed the ordinary way.
    {
        let mut s = Store::open_encrypted(&dir, FoldCfg { block_target: 8 * 1024, ..Default::default() }, c.clone())
            .unwrap();
        let ids: Vec<String> = alice.iter().map(|(id, _)| id.clone()).collect();
        let out = s.erase_scope("subject:alice", &ids).unwrap();
        assert!(out.key_destroyed, "the scope's key must be destroyed");
        assert_eq!(out.records.tombstoned, 6);
        assert!(out.records.refold.unwrap().pieces_dropped > 0, "local bytes must go too");
    }

    // Bob is untouched and byte-exact; alice is gone at every layer.
    {
        let s = Store::open_encrypted(&dir, FoldCfg { block_target: 8 * 1024, ..Default::default() }, c.clone())
            .unwrap();
        for (id, body) in &bob {
            assert_eq!(&s.reconstruct(id).unwrap().unwrap(), body, "{id} damaged by another scope's erasure");
        }
        for (id, _) in &alice {
            assert!(s.reconstruct(id).unwrap().is_none(), "{id} must be gone");
            // ... and not merely shadowed: no part carries the row at all
            for p in s.parts() {
                assert!(p.find(id).unwrap().is_none(), "{id} survives as a row in a part");
            }
        }
    }

    // The key is gone for good: a FRESH cipher over the same keyring cannot recover it, which is
    // what makes the claim reach copies this process never sees.
    {
        let c2 = cipher(&d);
        assert!(!c2.destroy_subject("subject:alice").unwrap(), "alice's key must already be gone");
        assert!(c2.destroy_subject("subject:bob").unwrap(), "bob's key must still exist");
    }
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn a_scope_change_seals_the_block_so_two_scopes_never_share_a_key() {
    let d = tmp("packing");
    let dir = d.join("store");
    let c = cipher(&d);
    let mut s = Store::open_encrypted(&dir, FoldCfg { block_target: 4 << 20, ..Default::default() }, c.clone())
        .unwrap();

    // Bodies far smaller than block_target: without sealing at the boundary they would share one
    // block — and therefore one key, making either erasure destroy both.
    let a = write_subject(&mut s, "subject:a", 2);
    let b = write_subject(&mut s, "subject:b", 2);
    s.sync().unwrap();
    s.flush().unwrap();

    // Resolved through the PART's piece dictionary: the fold's in-memory window is released at
    // every flush, so it is not the place to ask where committed content lives.
    let block_of = |s: &Store, id: &str| -> u32 {
        let rec = s.get(id).unwrap().unwrap();
        let h = rec.body.iter().find_map(|op| match op {
            turndb::BodyOp::Piece { hash, .. } => Some(*hash),
            _ => None,
        }).unwrap();
        for p in s.parts() {
            if let Some(loc) = p.lookup_piece(&h).unwrap() {
                return loc.block_id;
            }
        }
        panic!("piece for {id} is in no part");
    };
    let ab = block_of(&s, &a[0].0);
    let bb = block_of(&s, &b[0].0);
    assert_ne!(ab, bb, "two scopes must not share a block: erasing one would erase the other");
    std::fs::remove_dir_all(&d).ok();
}
