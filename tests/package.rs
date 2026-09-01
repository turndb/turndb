//! What the published crate contains, asserted rather than remembered.
//!
//! The launch checklist claimed three user-facing examples ship. Six did — three dev instruments
//! were added over time and nobody updated the `exclude` list, because nothing checked. That is the
//! shape of every defect this repository found in its own documentation: a true sentence that
//! quietly stopped being true, with no mechanism to notice.
//!
//! So the count is not written down here. Every example is classified, and adding one fails this
//! test until somebody decides which it is — which is the only version that survives the next
//! person who is in a hurry.

use std::collections::BTreeSet;
use std::fs;

/// Examples that ship to crates.io: things a reader learns the API from.
///
/// A dev instrument measures or proves something about the engine and belongs in the repository,
/// not in a stranger's `cargo add`. Benchmarks and interop proofs are instruments; a worked mapping
/// of real records and the queries over it are examples.
const SHIPPED: &[&str] = &["genai_dogfood", "genai_query", "query_demo"];

fn stem(p: &str) -> String {
    p.trim_start_matches("examples/").trim_end_matches(".rs").to_string()
}

#[test]
fn every_example_is_either_shipped_or_deliberately_excluded() {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();

    // The `exclude` array, read from the manifest rather than duplicated here — duplicating it is
    // how the two would drift apart, which is the defect this test exists for.
    let excl_start = manifest.find("exclude = [").expect("Cargo.toml must declare `exclude`");
    let excl_end = excl_start + manifest[excl_start..].find(']').expect("unterminated exclude");
    let excluded: BTreeSet<String> = manifest[excl_start..excl_end]
        .split('"')
        .filter(|s| s.starts_with("examples/") && s.ends_with(".rs"))
        .map(stem)
        .collect();

    let on_disk: BTreeSet<String> = fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/examples"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".rs"))
        .map(|n| stem(&n))
        .collect();

    let shipped: BTreeSet<String> = SHIPPED.iter().map(|s| s.to_string()).collect();

    // Every example is classified exactly once. An unclassified one is the actual failure mode:
    // it ships by default, silently, because `exclude` is opt-out.
    let unclassified: Vec<&String> =
        on_disk.iter().filter(|e| !excluded.contains(*e) && !shipped.contains(*e)).collect();
    assert!(
        unclassified.is_empty(),
        "example(s) neither excluded from the package nor declared user-facing: {unclassified:?}\n\
         Add to `exclude` in Cargo.toml (dev instrument) or to SHIPPED here (worked example). \
         Unclassified examples ship to crates.io by default."
    );

    let both: Vec<&String> = shipped.intersection(&excluded).collect();
    assert!(both.is_empty(), "declared user-facing AND excluded, so it does not ship: {both:?}");

    let missing: Vec<&String> = shipped.iter().filter(|e| !on_disk.contains(*e)).collect();
    assert!(missing.is_empty(), "SHIPPED names an example that no longer exists: {missing:?}");
}

#[test]
fn node_engine_claims_are_closed_and_match_the_ci_majors() {
    let root = env!("CARGO_MANIFEST_DIR");
    let portable: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(format!("{root}/npm/turndb/package.json")).unwrap(),
    )
    .unwrap();
    let native: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(format!("{root}/bindings/node/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(portable["engines"]["node"], ">=22 <27");
    assert_eq!(native["engines"]["node"], ">=22 <27");
    assert_eq!(
        native["private"], true,
        "prebuild readiness must not bypass owner-approved publication"
    );
    assert_eq!(native["napi"]["binaryName"], "turndb");
    assert_eq!(
        native["napi"]["targets"],
        serde_json::json!(["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"]),
        "every configured target needs its own build, package, and runtime evidence"
    );
    // The selector must request exactly the platform package this tree builds, so derive the
    // expectation instead of writing the version twice. A literal here passes whenever the
    // literal happens to match and says nothing about whether the pin tracks the package: it
    // reads as a check on the pin while actually checking a constant. It also fails on every
    // correct version bump, which is the reverse of what it is for.
    assert_eq!(
        native["optionalDependencies"]["@turndb/native-linux-x64-gnu"], native["version"],
        "the selector's optionalDependencies pin must name its own version; a stale pin selects \
         a platform package that was never published at that version"
    );
    assert_eq!(
        native["optionalDependencies"]["@turndb/native-win32-x64-msvc"], native["version"],
        "the Windows selector pin must move in lockstep"
    );
    assert_eq!(native["exports"]["."]["types"], "./index.d.ts");
    assert_eq!(native["exports"]["."]["import"], "./index.mjs");
    assert_eq!(native["exports"]["."]["require"], "./index.cjs");
    for legal in ["LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.html"] {
        assert!(
            native["files"].as_array().unwrap().iter().any(|entry| entry == legal),
            "native root package must declare {legal} in its payload"
        );
    }

    let windows: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(format!("{root}/bindings/node/npm/win32-x64-msvc/package.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(windows["name"], "@turndb/native-win32-x64-msvc");
    assert_eq!(windows["version"], native["version"]);
    assert_eq!(windows["private"], true);
    assert_eq!(windows["main"], "turndb.win32-x64-msvc.node");
    assert_eq!(windows["os"], serde_json::json!(["win32"]));
    assert_eq!(windows["cpu"], serde_json::json!(["x64"]));
    assert!(windows.get("libc").is_none(), "Windows package has no libc selector");

    let linux: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(format!("{root}/bindings/node/npm/linux-x64-gnu/package.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(linux["name"], "@turndb/native-linux-x64-gnu");
    assert_eq!(linux["version"], native["version"]);
    assert_eq!(linux["private"], true);
    assert_eq!(linux["main"], "turndb.linux-x64-gnu.node");
    assert_eq!(linux["os"], serde_json::json!(["linux"]));
    assert_eq!(linux["cpu"], serde_json::json!(["x64"]));
    assert_eq!(linux["libc"], serde_json::json!(["glibc"]));
    for legal in ["LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.html"] {
        assert!(
            linux["files"].as_array().unwrap().iter().any(|entry| entry == legal),
            "native platform package must declare {legal} in its payload"
        );
    }

    // Normalised: a checkout that converted the file to CRLF must not fail the exact-line search.
    let ci = fs::read_to_string(format!("{root}/.github/workflows/ci.yml"))
        .unwrap()
        .replace("\r\n", "\n");
    let portable_job = ci_job_block(&ci, "npm");
    let native_job = ci_job_block(&ci, "native-node");
    for (name, job) in [("portable", portable_job), ("native", native_job)] {
        assert!(
            job.contains("node: ['22', '24', '26']"),
            "{name} package range and CI majors drifted"
        );
        assert!(!job.contains("node: ['18'"), "{name} CI resurrected an EOL major");
        assert!(!job.contains("node: ['20'"), "{name} CI resurrected an EOL major");
    }

    let prebuild_install_job = ci_job_block(&ci, "native-prebuild-install");
    assert!(prebuild_install_job.contains("needs: native-prebuild"));
    assert!(prebuild_install_job.contains("node: ['22', '24', '26']"));
    assert!(prebuild_install_job.contains("scripts/test-prebuild.cjs"));
}

fn guarded_python_publish_job(workflow: &str) -> Result<&str, String> {
    let key = "  python_publish:\n";
    if workflow.matches(key).count() != 1 {
        return Err("release.yml must contain exactly one python_publish job".into());
    }
    let start = workflow.find(key).unwrap() + key.len();
    let rest = &workflow[start..];
    let end = rest
        .lines()
        .scan(0usize, |offset, line| {
            let here = *offset;
            *offset += line.len() + 1;
            Some((here, line))
        })
        .find_map(|(offset, line)| {
            (line.starts_with("  ") && !line.starts_with("    ") && line.trim_end().ends_with(':'))
                .then_some(offset)
        })
        .unwrap_or(rest.len());
    let job = &rest[..end];
    if job.matches("uses: actions/download-artifact@").count() != 1 {
        return Err("python_publish must have exactly one artifact download".into());
    }
    if !job.contains("pattern: turndb-python-*-${{ needs.tag.outputs.release_ref }}")
        || !job.contains("path: dist")
    {
        return Err(
            "python_publish must download only the release-python artifact family to dist".into()
        );
    }
    if job.matches("uses: pypa/gh-action-pypi-publish@").count() != 1
        || !job.contains("packages-dir: dist/")
    {
        return Err("python_publish must publish only the downloaded dist directory".into());
    }
    if job.matches("      - uses:").count() != 2 {
        return Err("python_publish may contain only download and publish actions".into());
    }
    for forbidden in [
        "actions/checkout@",
        "actions/setup-python@",
        "dtolnay/rust-toolchain@",
        " run:",
        "maturin",
        "cargo ",
        "pip ",
        "curl ",
    ] {
        if job.contains(forbidden) {
            return Err(format!("python_publish contains byte-producing step {forbidden:?}"));
        }
    }
    Ok(job)
}

#[test]
fn pypi_publish_cannot_bypass_the_install_verified_artifact_handoff() {
    let root = env!("CARGO_MANIFEST_DIR");
    let workflow = fs::read_to_string(format!("{root}/.github/workflows/release.yml")).unwrap();
    guarded_python_publish_job(&workflow).unwrap();

    // Prove the guard can reject the bypass it claims to detect. A matcher that has never rejected
    // anything can pass forever after the job it watches is renamed or its shape changes.
    let rebuild_shaped = r#"  python_publish:
    steps:
      - uses: actions/download-artifact@v4
        with:
          pattern: turndb-python-*-${{ needs.tag.outputs.release_ref }}
          path: dist
      - uses: actions/checkout@v7
      - run: maturin build --out dist
      - uses: pypa/gh-action-pypi-publish@release/v1
        with:
          packages-dir: dist/
  next_job:
    steps: []
"#;
    assert!(
        guarded_python_publish_job(rebuild_shaped).is_err(),
        "the frozen-caller guard accepted a checkout-and-rebuild bypass"
    );
    assert!(
        guarded_python_publish_job("  renamed_python_publish:\n    steps: []\n").is_err(),
        "the frozen-caller guard accepted an absent/renamed job"
    );
}

/// A single job's block from a workflow file, bounded by the next top-level job key.
///
/// This used to bound each job by whichever job was written after it — `npm` ended where
/// `native-node` began, `native-prebuild-install` ended where `dst` began. That couples a claim
/// about the Node matrix to the *order and membership* of every other job, so moving `dst` into
/// `nightly.yml` failed this test with `CI must install-test the collected native prebuild
/// separately` — a message describing a defect that did not exist. A check that fails for a reason
/// unrelated to what it asserts is a check nobody can act on.
fn ci_job_block<'a>(ci: &'a str, name: &str) -> &'a str {
    let key = format!("  {name}:\n");
    let start =
        ci.find(&key).unwrap_or_else(|| panic!("CI must retain a distinct {name} job")) + key.len();
    let rest = &ci[start..];
    let mut offset = 0usize;
    for line in rest.lines() {
        let is_top_level_key =
            line.starts_with("  ") && !line.starts_with("   ") && line.trim_end().ends_with(':');
        if is_top_level_key {
            return &rest[..offset];
        }
        offset += line.len() + 1;
    }
    rest
}

#[test]
fn native_release_is_explicitly_owner_gated() {
    let root = env!("CARGO_MANIFEST_DIR");
    let workflow =
        fs::read_to_string(format!("{root}/.github/workflows/release-native.yml")).unwrap();
    assert!(workflow.contains("workflow_dispatch:"));
    assert!(workflow.contains("release_ref:"));
    for node in ["node: '22'", "node: '24'", "node: '26'"] {
        assert!(workflow.contains(node), "native release lost install coverage for {node}");
    }
    assert!(workflow.contains("runner: windows-latest"));
    assert!(workflow.contains("needs: [build, install]"));
    assert!(workflow.contains("environment: npm"));
    assert!(workflow.contains("id-token: write"));
    assert!(workflow.contains("package:pack:release"));
    assert!(workflow.contains("scripts/publish-prebuild.cjs"));
    assert!(workflow.contains("on:\n  workflow_call:"));
    assert!(workflow.contains("  workflow_dispatch:"));
    assert!(!workflow.contains("  push:"));
    assert!(!workflow.contains("  pull_request:"));

    let publisher =
        fs::read_to_string(format!("{root}/bindings/node/scripts/publish-prebuild.cjs")).unwrap();
    for guard in [
        "GITHUB_ACTIONS",
        "TURNDB_RELEASE_APPROVED",
        "publishable !== true",
        "--exact-match",
        "cat-file",
        "--provenance",
    ] {
        assert!(publisher.contains(guard), "native publisher lost guard {guard}");
    }
    assert!(
        publisher.contains("for (const tarball of [...platformTarballs, selectorTarball])"),
        "every platform package must publish before the root selector package"
    );
}

#[test]
fn cli_release_is_explicitly_owner_gated() {
    let root = env!("CARGO_MANIFEST_DIR");
    let workflow = fs::read_to_string(format!("{root}/.github/workflows/release-cli.yml")).unwrap();
    assert!(workflow.contains("on:\n  workflow_call:"));
    assert!(workflow.contains("  workflow_dispatch:"));
    assert!(!workflow.contains("  push:"));
    assert!(!workflow.contains("  pull_request:"));
    assert!(workflow.contains("environment: npm"));
    assert!(workflow.contains("    permissions:\n      contents: read\n      id-token: write"));
    assert!(workflow.contains("scripts/publish-cli.cjs"));

    let publisher = fs::read_to_string(format!("{root}/cli/scripts/publish-cli.cjs")).unwrap();
    for guard in
        ["GITHUB_ACTIONS", "TURNDB_RELEASE_APPROVED", "--exact-match", "cat-file", "--provenance"]
    {
        assert!(publisher.contains(guard), "CLI publisher lost guard {guard}");
    }
}

#[test]
fn failed_release_retries_build_the_intended_package_through_the_trusted_caller() {
    let root = env!("CARGO_MANIFEST_DIR");
    let wasm_leaf =
        fs::read_to_string(format!("{root}/.github/workflows/release-wasm.yml")).unwrap();
    assert!(
        wasm_leaf.contains("open(join(dir, \"smoke.turndb\"))"),
        "the installed package smoke must open a store file, not its temporary parent directory"
    );
    assert!(
        !wasm_leaf.contains("open(dir)"),
        "the retired directory layout must not return through a release smoke"
    );

    let python_leaf =
        fs::read_to_string(format!("{root}/.github/workflows/release-python.yml")).unwrap();
    assert!(
        ci_job_block(&python_leaf, "build").contains("--manifest-path bindings/python/Cargo.toml"),
        "the release wheel must build the Python binding, not package the root CLI binary"
    );

    let caller = fs::read_to_string(format!("{root}/.github/workflows/release.yml")).unwrap();
    assert!(caller.contains("workflow_dispatch:"));
    assert!(caller.contains("component:"));
    assert!(caller.contains("REQUESTED_REF: ${{ inputs.release_ref }}"));
    assert!(caller.contains("git describe --tags --exact-match HEAD"));
    assert!(caller.contains("git cat-file -t \"$RELEASE_REF\""));
    assert!(caller.contains("gh release view \"$RELEASE_REF\""));

    for job in ["crate", "native", "browser", "cli"] {
        assert!(
            ci_job_block(&caller, job).contains("if: github.event_name == 'pull_request'"),
            "a failed-component retry must not fan out the {job} release"
        );
    }
    assert!(
        ci_job_block(&caller, "wasm").contains("inputs.component == 'wasm'"),
        "portable wasm recovery must be selected explicitly through the trusted caller"
    );
    assert!(
        ci_job_block(&caller, "python").contains("inputs.component == 'python'"),
        "Python recovery must be selected explicitly through the trusted caller"
    );
    let python_publish = ci_job_block(&caller, "python_publish");
    assert!(python_publish.contains("needs: [tag, python]"));
    assert!(python_publish.contains("environment: pypi"));
    assert!(python_publish.contains("id-token: write"));
    assert!(python_publish.contains("pypa/gh-action-pypi-publish@release/v1"));
    assert!(
        !python_leaf.contains("pypa/gh-action-pypi-publish@release/v1"),
        "PyPI attestations cannot be issued from a reusable workflow until PyPI supports its split OIDC identities"
    );
}

/// A tripwire for "no engine path runs `reclaim`" — and it is a lint, not a gate.
///
/// The support doc tells a reader that reclaim's cost is paid only when they run the command.
/// That is true today because the engine has one caller: the CLI verb. It would stop being true
/// the day someone wires automatic reclamation into a commit path, and nothing else in the tree
/// would notice.
///
/// **What this catches:** a direct call, and an import that renames the function. **What it does
/// not catch:** a re-export, a function pointer, a macro, or any other indirection — this scans
/// source text, and source text is a spelling rather than a population. Seamus defeated the first
/// version of this check with `use crate::container::reclaim as compact_store;` in under a
/// minute, which is the correct verdict on how much it is worth: enough to catch the accident,
/// not enough for the prose to lean on. The doc sentence says so rather than claiming a gate.
#[test]
fn reclaim_has_one_caller() {
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    files.sort();

    // The one place allowed to call it. No line number: a call moving down the file is not a
    // finding, and a check that cries about it teaches people to ignore it.
    const CALLER: &str = "src/bin/turndb.rs";
    let mut offenders: Vec<String> = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(root)
            .unwrap()
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let text = std::fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            // Renaming the function is how the first version of this check was defeated, so the
            // rename is itself the thing to notice — wherever it appears.
            let aliased = code.contains("use ")
                && code.contains("reclaim")
                && code.contains(" as ")
                && !code.contains("reclaim_names");
            let called = (code.contains("reclaim(") || code.contains("reclaim_with_construction("))
                && !code.contains("fn reclaim")
                && !code.contains("reclaim_names(");
            if (aliased || called) && rel != CALLER {
                offenders.push(format!("{rel}:{}: {}", n + 1, code.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "reclaim gained a caller outside {CALLER}. If an engine path now runs it, the support \
         doc's \"no engine path runs reclaim\" is false and must change with it:\n  {}",
        offenders.join("\n  ")
    );
}
