//! The recorded wire-body golden fixtures (tests/fixtures/phase1_wire, asserted
//! byte-for-byte by tests/phase1_wire_golden.rs) must stay anchored to the
//! `phase-1-complete` commit they were captured from.

use std::process::Command;

/// The tag the golden fixtures were captured from.
const BASELINE: &str = "phase-1-complete^{commit}";

fn git(args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The MANIFEST `base_commit` is the dereferenced tag COMMIT (not the
/// annotated-tag object).
#[test]
fn locked_golden_fixtures_are_unchanged() {
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/phase1_wire/MANIFEST.txt"
    ))
    .expect("read MANIFEST");
    let base = manifest
        .lines()
        .find_map(|l| l.split("base_commit:").nth(1))
        .map(str::trim)
        .expect("MANIFEST has base_commit");
    let commit = git(&["rev-parse", BASELINE]);
    assert_eq!(
        base,
        commit.trim(),
        "MANIFEST base_commit must be the dereferenced phase-1-complete commit"
    );
}
