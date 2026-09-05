//! `turndb --version` reports the crate version compiled in — not a string that could drift from
//! it, and not "unknown verb" (#97). Installed from npm as `@turndb/cli`, the selector package, the
//! platform package and the binary are three separately versioned things; this is the one a bug
//! report can quote.

use std::process::Command;

fn turndb() -> Command {
    Command::new(env!("CARGO_BIN_EXE_turndb"))
}

#[test]
fn every_version_spelling_prints_the_compiled_crate_version() {
    let expected = format!("turndb {}\n", env!("CARGO_PKG_VERSION"));
    for spelling in ["--version", "-V", "version"] {
        let out = turndb().arg(spelling).output().unwrap();
        assert!(
            out.status.success(),
            "{spelling} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{spelling}");
        assert!(out.stderr.is_empty(), "{spelling} wrote to stderr");
    }
}

#[test]
fn help_mentions_the_version_verb() {
    let out = turndb().arg("help").output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("version"),
        "help does not list `version`"
    );
}

#[test]
fn help_names_backup_operation() {
    let out = turndb().arg("help").output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("backup    <STORE> <OUT>"), "help does not list `backup`: {help}");
}
