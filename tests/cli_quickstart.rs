//! The README quickstart, run the way a stranger runs it: from a fresh directory, with the store
//! named by a bare relative path (`mystore.turndb`), exactly as `README.md` shows it.
//!
//! #121: `Path::new("mystore.turndb").parent()` is `Some("")`, and fsyncing `""` is ENOENT, so the
//! first command of the quickstart failed and left an empty store and a WAL behind. The test
//! drives the built `turndb` binary as a subprocess with its working directory set, because a
//! test cannot change the test process's own directory without racing every other test.

use std::fs;
use std::path::Path;
use std::process::Command;

fn turndb() -> Command {
    Command::new(env!("CARGO_BIN_EXE_turndb"))
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir =
        std::env::temp_dir().join(format!("turndb-quickstart-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn quickstart_with_a_bare_store_name_imports_and_leaves_exactly_one_file() {
    let dir = tmp("bare");
    fs::write(
        dir.join("traces.jsonl"),
        "{\"body\":\"[1,2]\",\"model\":\"m1\"}\n{\"body\":\"[3]\",\"model\":\"m2\"}\n",
    )
    .unwrap();

    // README.md: `turndb import mystore.turndb -` — bare relative name, cwd is the user's directory.
    let out = turndb()
        .current_dir(&dir)
        .args(["import", "mystore.turndb", "traces.jsonl"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "import exited {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        entries(&dir),
        vec!["mystore.turndb".to_string(), "traces.jsonl".to_string()],
        "a cleanly closed store is exactly one file beside the input"
    );

    // The rest of the quickstart's read verbs, against the same bare name.
    for verb in [
        &["inspect", "mystore.turndb"][..],
        &["verify", "mystore.turndb", "--deep"][..],
        &["ids", "mystore.turndb"][..],
    ] {
        let out = turndb().current_dir(&dir).args(verb).output().unwrap();
        assert!(
            out.status.success(),
            "{verb:?} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let ids = turndb().current_dir(&dir).args(["ids", "mystore.turndb"]).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&ids.stdout).lines().count(),
        2,
        "both imported records are live"
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_dotted_relative_name_and_an_absolute_name_behave_the_same_as_a_bare_one() {
    let dir = tmp("forms");
    fs::write(dir.join("t.jsonl"), "{\"body\":\"[1]\"}\n").unwrap();
    let abs = dir.join("abs.turndb");
    for store in ["bare.turndb", "./dotted.turndb", abs.to_str().unwrap()] {
        let out = turndb().current_dir(&dir).args(["import", store, "t.jsonl"]).output().unwrap();
        assert!(
            out.status.success(),
            "import {store} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(
        entries(&dir),
        vec![
            "abs.turndb".to_string(),
            "bare.turndb".to_string(),
            "dotted.turndb".to_string(),
            "t.jsonl".to_string()
        ]
    );
    fs::remove_dir_all(&dir).unwrap();
}
