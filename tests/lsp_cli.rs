//! The editor entry point preserves the offline replay CLI contract.

use std::process::Command;
mod common;

fn command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(&mut command, root);
    command
}

const REPLAY_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/navigator/replay/mixed_log.jsonl"
);

fn assert_replay_report(stdout: Vec<u8>) {
    let report = withnav::navigator::replay::flows::report(
        &std::fs::read_to_string(REPLAY_FIXTURE).unwrap(),
    );
    assert_eq!(
        String::from_utf8(stdout).unwrap(),
        format!("{}\n", serde_json::to_string_pretty(&report).unwrap())
    );
}

#[test]
fn replay_json_preserves_the_report() {
    let root = tempfile::tempdir().unwrap();
    let output = command(root.path())
        .args(["lsp", "replay", "--log", REPLAY_FIXTURE, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_replay_report(output.stdout);
}

#[test]
fn stdio_flag_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    let output = command(root.path())
        .args(["lsp", "--stdio", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("--stdio")
    );

    // Execute a real offline command too: help alone bypasses command execution.
    let output = command(root.path())
        .args([
            "lsp",
            "--stdio",
            "replay",
            "--log",
            REPLAY_FIXTURE,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_replay_report(output.stdout);
}
