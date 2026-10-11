//! The editor entry point preserves the offline replay CLI contract.

use std::process::Command;

#[test]
fn replay_json_preserves_the_report() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/navigator/replay/mixed_log.jsonl"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_nav"))
        .args(["lsp", "replay", "--log", fixture, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let report =
        withnav::navigator::replay::flows::report(&std::fs::read_to_string(fixture).unwrap());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", serde_json::to_string_pretty(&report).unwrap())
    );
}

#[test]
fn stdio_flag_is_accepted() {
    let output = Command::new(env!("CARGO_BIN_EXE_nav"))
        .args(["lsp", "--stdio", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8(output.stdout)
        .unwrap()
        .contains("--stdio"));
}
