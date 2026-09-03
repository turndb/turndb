//! Every CLI verb that takes the writer role leaves a cleanly closed store behind: exactly one
//! file, no `<store>-wal` sidecar. README.md and FORMAT.md both promise "a cleanly closed store is
//! exactly one file"; #122 found `backup`, `compact`, `refold`, `punch` and `erase` opening a writer
//! and returning without closing it, so each left a 0-byte WAL beside the store.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn turndb() -> Command {
    Command::new(env!("CARGO_BIN_EXE_turndb"))
}

fn fresh_store(tag: &str) -> (PathBuf, PathBuf) {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir =
        std::env::temp_dir().join(format!("turndb-lifecycle-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("t.jsonl"),
        "{\"body\":\"[1,2]\",\"k\":\"a\"}\n{\"body\":\"[3]\",\"k\":\"b\"}\n",
    )
    .unwrap();
    let store = dir.join("s.turndb");
    let out = turndb()
        .args(["import", store.to_str().unwrap(), dir.join("t.jsonl").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "import: {}", String::from_utf8_lossy(&out.stderr));
    fs::remove_file(dir.join("t.jsonl")).unwrap();
    (dir, store)
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn run(dir: &Path, args: &[&str]) {
    let out = turndb().output_with(args);
    let _ = dir;
    assert!(
        out.status.success(),
        "{args:?} exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

trait OutputWith {
    fn output_with(&mut self, args: &[&str]) -> std::process::Output;
}
impl OutputWith for Command {
    fn output_with(&mut self, args: &[&str]) -> std::process::Output {
        self.args(args).output().unwrap()
    }
}

#[test]
fn compact_refold_punch_erase_leave_exactly_one_file() {
    for verb in [&["compact"][..], &["refold"][..], &["punch"][..], &["erase", "--id", "r/1"][..]] {
        let (dir, store) = fresh_store(verb[0]);
        let mut args: Vec<&str> = vec![verb[0], store.to_str().unwrap()];
        args.extend_from_slice(&verb[1..]);
        run(&dir, &args);
        assert_eq!(
            entries(&dir),
            vec!["s.turndb".to_string()],
            "after `{}` the store is exactly one file",
            verb[0]
        );
        // Still a healthy store afterwards.
        run(&dir, &["verify", store.to_str().unwrap(), "--deep"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn backup_leaves_the_source_as_exactly_one_file_beside_the_copy() {
    let (dir, store) = fresh_store("backup");
    let out = dir.join("snap.turndb");
    run(&dir, &["backup", store.to_str().unwrap(), out.to_str().unwrap()]);
    assert_eq!(entries(&dir), vec!["s.turndb".to_string(), "snap.turndb".to_string()]);
    run(&dir, &["verify", out.to_str().unwrap(), "--deep"]);
    fs::remove_dir_all(&dir).unwrap();
}
