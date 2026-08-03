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
    assert_eq!(native["private"], true, "the native addon has no published prebuild contract yet");

    let ci = fs::read_to_string(format!("{root}/.github/workflows/ci.yml")).unwrap();
    let portable_job = ci
        .split_once("  npm:\n")
        .and_then(|(_, rest)| rest.split_once("  native-node:\n").map(|(job, _)| job))
        .expect("CI must retain a distinct portable npm job");
    let native_job = ci
        .split_once("  native-node:\n")
        .and_then(|(_, rest)| rest.split_once("  dst:\n").map(|(job, _)| job))
        .expect("CI must retain a distinct native Node job");
    for (name, job) in [("portable", portable_job), ("native", native_job)] {
        assert!(
            job.contains("node: ['22', '24', '26']"),
            "{name} package range and CI majors drifted"
        );
        assert!(!job.contains("node: ['18'"), "{name} CI resurrected an EOL major");
        assert!(!job.contains("node: ['20'"), "{name} CI resurrected an EOL major");
    }
}
