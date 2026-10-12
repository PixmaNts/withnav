//! Test support shared by the integration tests. Each `tests/*.rs` file is
//! its own crate and pulls this in with `mod common;`, so a helper one file
//! does not use is dead code there: hence the crate-level allow.
#![allow(dead_code)]

pub mod deadline;
pub mod navigator;
pub mod server;
pub mod sse;
pub mod watchdog;

/// Route a child binary away from the user's Known Folders as well as XDG/HOME.
/// WITHNAV_PREVIEW_TEST_ROOT is debug-only and Windows-only in the binary.
pub fn isolate(command: &mut std::process::Command, root: &std::path::Path) {
    assert!(root.is_absolute(), "subprocess test root must be absolute");
    command
        .env("WITHNAV_PREVIEW_TEST_ROOT", root)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"));
    #[cfg(windows)]
    {
        let mut probe = std::process::Command::new(env!("CARGO_BIN_EXE_nav"));
        probe
            .args(["doctor", "--offline", "--json"])
            .env("WITHNAV_PREVIEW_TEST_ROOT", root)
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"));
        let output = probe.output().expect("probe isolated child paths");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)
            .expect("doctor reports isolated child config location");
        let config = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "config.location")
            .unwrap();
        assert_eq!(
            std::path::Path::new(config["value"].as_str().unwrap()),
            root.join("config/withnav")
        );
    }
}

pub fn isolate_tokio(command: &mut tokio::process::Command, root: &std::path::Path) {
    isolate(command.as_std_mut(), root);
}

pub fn nav_command(root: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_nav"));
    isolate(&mut command, root);
    command
}

/// An isolated canonical root with empty workspace and home directories.
pub struct TestEnv {
    _tmp: tempfile::TempDir,
    pub root: std::path::PathBuf,
    pub ws: std::path::PathBuf,
    pub home: std::path::PathBuf,
}

impl Default for TestEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl TestEnv {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonical root");
        let ws = root.join("ws");
        let home = root.join("home");
        std::fs::create_dir_all(&ws).expect("workspace dir");
        std::fs::create_dir_all(&home).expect("home dir");
        Self {
            _tmp: tmp,
            root,
            ws,
            home,
        }
    }
}
