//! The recorded wire-body golden fixtures (tests/fixtures/phase1_wire, asserted
//! byte-for-byte by tests/phase1_wire_golden.rs) must match the SHA-256 digests
//! listed in their MANIFEST, so changing a fixture is a deliberate two-file edit.
//!
//! The check reads only the working tree: it used to resolve a git tag that was
//! never pushed, so it failed on every fresh clone and in CI.

use sha2::{Digest, Sha256};

#[test]
fn locked_golden_fixtures_are_unchanged() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/phase1_wire");
    let manifest = std::fs::read_to_string(format!("{dir}/MANIFEST.txt")).expect("read MANIFEST");
    let mut listed = 0;
    for line in manifest.lines() {
        let Some(rest) = line.strip_prefix("SHA-256") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let (Some(name), Some(expected)) = (fields.next(), fields.next()) else {
            panic!("malformed MANIFEST line: {line}");
        };
        let bytes = std::fs::read(format!("{dir}/{name}")).expect("read fixture");
        let actual = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(
            actual, expected,
            "{name} changed: update its SHA-256 in MANIFEST.txt only if the wire change is intended"
        );
        listed += 1;
    }
    let fixtures = std::fs::read_dir(dir)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        })
        .count();
    assert_eq!(
        listed, fixtures,
        "every fixture must be listed in MANIFEST.txt"
    );
}
