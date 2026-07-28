//! The constellation gate: many stores read as one, sealed packs and live directories side by
//! side, later members winning — which is the overlay pattern with no new machinery.

use std::path::PathBuf;
use turndb::catalog::{Catalog, CatalogReader, Member};
use turndb::fold::FoldCfg;
use turndb::store::{Span, Store};

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("turndb-cat-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg() -> FoldCfg {
    FoldCfg { block_target: 8 * 1024, ..Default::default() }
}

/// A member store holding `ids`, each with a body derived from `tag`.
fn member(root: &PathBuf, name: &str, tag: &str, ids: &[&str]) -> Vec<(String, Vec<u8>)> {
    let mut s = Store::open(&root.join(name), cfg()).unwrap();
    let mut want = Vec::new();
    for id in ids {
        let body = format!("{{\"member\":\"{tag}\",\"id\":\"{id}\",\"pad\":\"{}\"}}", "b".repeat(400));
        s.put(id, &[Span::Piece(body.as_bytes())], vec![]).unwrap();
        want.push((id.to_string(), body.into_bytes()));
    }
    s.sync().unwrap();
    s.flush().unwrap();
    want
}

#[test]
fn a_constellation_reads_as_one_store_across_directories_and_packs() {
    let root = tmp("mixed");
    let w30 = member(&root, "w30", "w30", &["a", "b", "c"]);
    let w31 = member(&root, "w31", "w31", &["d", "e"]);

    // seal the older window into a PACK — a member is a directory or a file, and the reader must
    // not care which
    turndb::pack::write(&root.join("w30"), &root.join("w30.turndb")).unwrap();
    std::fs::remove_dir_all(root.join("w30")).unwrap();

    let mut c = Catalog::default();
    c.add(Member { path: "w30.turndb".into(), ordinal: 0, window: Some("2026-W30".into()), sealed: true })
        .unwrap();
    c.add(Member { path: "w31".into(), ordinal: 1, window: Some("2026-W31".into()), sealed: false })
        .unwrap();
    c.commit(&root).unwrap();

    let r = CatalogReader::open(&root, cfg()).unwrap();
    assert_eq!(r.member_count(), 2);
    assert_eq!(r.ids().unwrap(), vec!["a", "b", "c", "d", "e"]);
    for (id, body) in w30.iter().chain(w31.iter()) {
        assert_eq!(&r.reconstruct(id).unwrap().unwrap(), body, "{id} lost across the constellation");
    }
    assert_eq!(r.locate("a").unwrap().unwrap(), "w30.turndb", "packed member must answer for its ids");
    assert_eq!(r.locate("d").unwrap().unwrap(), "w31");
    assert!(r.reconstruct("nonexistent").unwrap().is_none());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_later_member_overrides_a_sealed_one_which_is_the_overlay_pattern() {
    let root = tmp("overlay");
    let base = member(&root, "base", "base", &["k1", "k2"]);
    turndb::pack::write(&root.join("base"), &root.join("base.turndb")).unwrap();
    std::fs::remove_dir_all(root.join("base")).unwrap();

    // the OVERLAY: a tiny live member carrying a correction to a record inside the sealed pack
    let corrected = {
        let mut s = Store::open(&root.join("overlay"), cfg()).unwrap();
        let body = b"{\"member\":\"overlay\",\"id\":\"k1\",\"corrected\":true}".to_vec();
        s.put("k1", &[Span::Piece(&body)], vec![]).unwrap();
        s.sync().unwrap();
        s.flush().unwrap();
        body
    };

    let mut c = Catalog::default();
    c.add(Member { path: "base.turndb".into(), ordinal: 0, window: None, sealed: true }).unwrap();
    c.add(Member { path: "overlay".into(), ordinal: 1, window: None, sealed: false }).unwrap();
    c.commit(&root).unwrap();

    let r = CatalogReader::open(&root, cfg()).unwrap();
    assert_eq!(r.reconstruct("k1").unwrap().unwrap(), corrected, "the later member must win");
    assert_eq!(r.locate("k1").unwrap().unwrap(), "overlay");
    // and the untouched record still comes from the sealed pack
    assert_eq!(r.reconstruct("k2").unwrap().unwrap(), base[1].1);
    assert_eq!(r.locate("k2").unwrap().unwrap(), "base.turndb");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn retention_is_removal_and_the_catalog_rebuilds_from_the_members() {
    let root = tmp("retention");
    member(&root, "w29", "w29", &["old1", "old2"]);
    let keep = member(&root, "w30", "w30", &["new1"]);

    let mut c = Catalog::default();
    c.add(Member { path: "w29".into(), ordinal: 0, window: Some("2026-W29".into()), sealed: true }).unwrap();
    c.add(Member { path: "w30".into(), ordinal: 1, window: Some("2026-W30".into()), sealed: false }).unwrap();
    c.commit(&root).unwrap();

    // RETENTION: expire everything before W30 — the catalog drops it, then the bytes go. Two
    // steps on purpose, so an operator can stage an expiry and still change their mind.
    let expired: Vec<String> = c.in_window(|w| w < "2026-W30").iter().map(|m| m.path.clone()).collect();
    assert_eq!(expired, vec!["w29".to_string()]);
    for p in &expired {
        assert!(c.remove(p));
    }
    c.commit(&root).unwrap();
    let r = CatalogReader::open(&root, cfg()).unwrap();
    assert_eq!(r.ids().unwrap(), vec!["new1"], "an expired window must be invisible");
    assert!(r.reconstruct("old1").unwrap().is_none());
    drop(r);
    for p in &expired {
        std::fs::remove_dir_all(root.join(p)).unwrap(); // ... and now it is gone for good
    }

    // REBUILD: the catalog is derived, so losing it is an inconvenience
    std::fs::remove_file(root.join("CATALOG")).unwrap();
    let rebuilt = Catalog::rebuild(&root).unwrap();
    assert_eq!(rebuilt.members.len(), 1);
    assert_eq!(rebuilt.members[0].path, "w30");
    assert!(rebuilt.members[0].window.is_none(), "a rebuild cannot invent policy it never stored");

    let mut rebuilt = rebuilt;
    rebuilt.commit(&root).unwrap();
    let r = CatalogReader::open(&root, cfg()).unwrap();
    assert_eq!(r.reconstruct("new1").unwrap().unwrap(), keep[0].1, "rebuilt catalog still reads");
    std::fs::remove_dir_all(&root).ok();
}
