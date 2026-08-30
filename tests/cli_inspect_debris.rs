//! `turndb inspect` prints the transient names beside a store BEFORE anything is opened, so
//! debris beside an absent store, or beside a directory, is listed and then the command errors
//! as it always did (obj-mtg0jtf1-l, outcome d).
use std::path::PathBuf;
use std::process::Command;

fn turndb() -> Command {
    Command::new(env!("CARGO_BIN_EXE_turndb"))
}

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "turndb-cli-debris-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn inspect_lists_debris_beside_an_absent_store_before_it_errors() {
    let d = scratch("absent");
    let store = d.join("s.turndb");
    std::fs::write(d.join("s.turndb.publish-7-1"), b"x").unwrap();
    std::fs::write(d.join("s.turndb.reclaim-candidate"), b"x").unwrap();
    let out = turndb().arg("inspect").arg(&store).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "an absent store is still an error");
    assert!(stdout.contains("debris: 2 transient file(s)"), "{stdout}");
    assert!(
        stdout.contains("s.turndb.publish-7-1") && stdout.contains("PendingPublish"),
        "{stdout}"
    );
    assert!(
        stdout.contains("s.turndb.reclaim-candidate") && stdout.contains("ReclaimCandidate"),
        "{stdout}"
    );
    assert!(d.join("s.turndb.publish-7-1").exists(), "inspect never removes");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn inspect_lists_debris_in_a_directory_layout_then_refuses_the_directory() {
    let d = scratch("dir");
    std::fs::write(d.join("MANIFEST.tmp"), b"x").unwrap();
    std::fs::write(d.join("part-00000004.part.s2.tmp"), b"x").unwrap();
    let out = turndb().arg("inspect").arg(&d).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stdout.contains("debris: 2 transient file(s)"), "{stdout}");
    assert!(stdout.contains("ManifestStaging") && stdout.contains("PartBuilderSpool"), "{stdout}");
    assert!(stderr.contains("convert"), "the directory refusal names the one door: {stderr}");
    assert!(d.join("MANIFEST.tmp").exists(), "inspect never removes");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn inspect_lists_a_legacy_working_directory_which_nothing_removes() {
    let d = scratch("hot");
    let store = d.join("s.turndb");
    std::fs::write(&store, b"not a store yet, just bytes").unwrap();
    std::fs::create_dir_all(d.join("s.turndb-hot")).unwrap();
    std::fs::write(d.join("s.turndb-hot").join("WAL"), b"acked").unwrap();
    let out = turndb().arg("inspect").arg(&store).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("s.turndb-hot") && stdout.contains("LegacyHotDirectory"), "{stdout}");
    assert!(d.join("s.turndb-hot").join("WAL").exists(), "inspect never removes");
    let _ = std::fs::remove_dir_all(&d);
}
