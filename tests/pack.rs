//! The pack gate: a store in one file answers exactly as the directory did, and both crossings
//! are mechanical.

use std::path::{Path, PathBuf};
use turndb::fold::FoldCfg;
use turndb::store::{Span, Store};
use turndb::AttrValue;

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turndb-pack-{tag}-{}-{n}", std::process::id()))
}

fn cfg() -> FoldCfg {
    // small blocks and segments so the pack carries a multi-segment fold with sidecars
    FoldCfg { block_target: 4 * 1024, seg_max: 16 * 1024, ..Default::default() }
}

/// Deterministic incompressible bytes — segments must actually roll.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut h = blake3::hash(&seed.to_le_bytes());
    while out.len() < len {
        out.extend_from_slice(h.as_bytes());
        h = blake3::hash(h.as_bytes());
    }
    out.truncate(len);
    out
}

/// A store with several flush intervals, a merge, a delete, and enough content to roll segments.
fn build_store(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut s = Store::open(dir, cfg()).unwrap();
    let mut want = Vec::new();
    for round in 0..3 {
        for i in 0..12 {
            let id = format!("r{round}:{i:02}");
            let body = noise(round as u64 * 100 + i as u64, 1800);
            s.put(
                &id,
                &[Span::Lit(b"["), Span::Piece(&body), Span::Lit(b"]")],
                vec![
                    ("model".into(), AttrValue::Str(format!("m{}", i % 2))),
                    ("n".into(), AttrValue::Int(i)),
                ],
            )
            .unwrap();
            let mut w = b"[".to_vec();
            w.extend_from_slice(&body);
            w.extend_from_slice(b"]");
            want.push((id, w));
        }
        s.sync().unwrap();
        s.flush().unwrap();
    }
    s.delete("r0:00").unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    want.retain(|(id, _)| id != "r0:00");
    s.merge_range(0, 2).unwrap().unwrap();
    assert!(s.fold().segment_count() > 1, "the fixture must roll at least one segment");
    want
}

#[test]
fn a_pack_answers_identically_to_the_directory_it_came_from() {
    let root = tmp("roundtrip");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    let want = build_store(&dir);

    let pk = root.join("snapshot.turndb");
    let stats = turndb::pack::write(&dir, &pk).unwrap();
    assert!(stats.files > 3, "manifest + parts + segments: {stats:?}");
    assert_eq!(turndb::pack::Pack::open(&pk).unwrap().verify().unwrap(), stats.files);

    let from_dir = Store::open_read(&dir, cfg()).unwrap();
    let from_pack = turndb::store::open_read_pack(&pk, cfg()).unwrap();
    assert_eq!(from_dir.ids().unwrap(), from_pack.ids().unwrap());
    for (id, body) in &want {
        assert_eq!(
            from_pack.reconstruct(id).unwrap().unwrap(),
            *body,
            "{id} must reconstruct identically out of the pack"
        );
        let a = from_dir.get(id).unwrap().unwrap();
        let b = from_pack.get(id).unwrap().unwrap();
        assert_eq!(a, b, "{id} record must match field for field");
    }
    assert!(from_pack.reconstruct("r0:00").unwrap().is_none(), "the delete holds inside the pack");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn unpacking_yields_an_ordinary_writable_store() {
    let root = tmp("unpack");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    let want = build_store(&dir);

    let pk = root.join("snapshot.turndb");
    turndb::pack::write(&dir, &pk).unwrap();
    let out = root.join("restored");
    turndb::pack::unpack(&pk, &out).unwrap();

    // The restored directory is a full store: readable, and the WRITER ROLE is available again.
    let mut s = Store::open(&out, cfg()).unwrap();
    for (id, body) in &want {
        assert_eq!(s.reconstruct(id).unwrap().unwrap(), *body, "{id} lost in the crossing");
    }
    s.put("after:restore", &[Span::Piece(b"written after unpacking, long enough to fold")], vec![])
        .unwrap();
    s.sync().unwrap();
    s.flush().unwrap();
    assert!(s.reconstruct("after:restore").unwrap().is_some());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_pack_refuses_what_it_must() {
    let root = tmp("refuse");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("store");
    build_store(&dir);

    // Uncommitted records must refuse the pack, not silently vanish from it.
    {
        let mut s = Store::open(&dir, cfg()).unwrap();
        s.put("unflushed", &[Span::Lit(b"x")], vec![]).unwrap();
        s.sync().unwrap();
        assert!(
            turndb::pack::write(&dir, &root.join("no.turndb")).is_err(),
            "a non-empty WAL must refuse packing"
        );
        s.flush().unwrap();
    }
    let pk = root.join("snapshot.turndb");
    turndb::pack::write(&dir, &pk).unwrap();
    let pristine = std::fs::read(&pk).unwrap();

    // torn footer
    std::fs::write(&pk, &pristine[..pristine.len() - 7]).unwrap();
    assert!(turndb::pack::Pack::open(&pk).is_err(), "a torn footer must refuse");

    // future version
    let mut b = pristine.clone();
    let vat = b.len() - 40 + 29;
    b[vat] = 9;
    // re-seal the footer checksum so ONLY the version differs
    let foot_start = b.len() - 40;
    let x = blake3::hash(&b[foot_start..foot_start + 36]);
    let xat = b.len() - 4;
    b[xat..].copy_from_slice(&x.as_bytes()[0..4]);
    std::fs::write(&pk, &b).unwrap();
    let err = match turndb::pack::Pack::open(&pk) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("must refuse"),
    };
    assert!(err.contains("version"), "a future version must refuse by name, got: {err}");

    // reserved bytes are enforced, not decorative
    let mut b = pristine.clone();
    let rat = b.len() - 40 + 30;
    b[rat] = 1;
    let x = blake3::hash(&b[foot_start..foot_start + 36]);
    b[xat..].copy_from_slice(&x.as_bytes()[0..4]);
    std::fs::write(&pk, &b).unwrap();
    let err = match turndb::pack::Pack::open(&pk) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("must refuse"),
    };
    assert!(err.contains("reserved"), "non-zero reserved bytes must refuse, got: {err}");
    std::fs::remove_dir_all(&root).ok();
}
