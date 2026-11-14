//! Stops the background owner a test's `nav` subprocesses started in an isolated data dir, so no
//! test leaves a detached server running.
use std::path::{Path, PathBuf};

/// On drop, kills the owner published under `<data dir>/server/owner.json`, if any.
pub struct OwnerGuard {
    record: PathBuf,
}

impl OwnerGuard {
    /// `data_dir` is the value given to the subprocesses as `WITHNAV_DATA_DIR`
    /// (`<root>/data/withnav` with [`super::isolate`]).
    pub fn new(data_dir: &Path) -> Self {
        Self {
            record: data_dir.join("server").join("owner.json"),
        }
    }

    /// The pid of the published owner, if one is published.
    pub fn pid(&self) -> Option<u32> {
        let bytes = std::fs::read(&self.record).ok()?;
        let record: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        record["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok())
    }
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        let Some(pid) = self.pid() else { return };
        #[cfg(unix)]
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
    }
}
