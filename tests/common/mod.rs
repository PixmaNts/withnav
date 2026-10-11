//! Test support shared by the integration tests. Each `tests/*.rs` file is
//! its own crate and pulls this in with `mod common;`, so a helper one file
//! does not use is dead code there: hence the crate-level allow.
#![allow(dead_code)]

pub mod deadline;
pub mod navigator;
pub mod server;
pub mod sse;
pub mod watchdog;

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
