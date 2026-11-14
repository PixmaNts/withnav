//! Stops the background owner a test's `nav` subprocesses started in an isolated data dir, so no
//! test leaves a detached server running.
use std::path::{Path, PathBuf};

/// Set by [`super::isolate`] on every `nav` a test starts, and so inherited by the owner it may
/// start: the pid of the test runner.
pub const RUNNER_PID_ENV: &str = "WITHNAV_TEST_RUNNER_PID";

/// Stops background owners left by an earlier test run that was killed before its guards ran
/// (a timeout, an interrupted run). Only a `nav serve` that carries [`RUNNER_PID_ENV`] naming a
/// runner that is gone, and whose data directory is under the system temporary directory, is
/// stopped: a real owner never has that variable, and a live run's owners are left alone. No
/// file is removed.
pub fn sweep_orphans() {
    #[cfg(target_os = "linux")]
    {
        let Ok(processes) = std::fs::read_dir("/proc") else {
            return;
        };
        let temp = std::env::temp_dir();
        for entry in processes.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            if !args.iter().any(|a| *a == b"serve") || !args.iter().any(|a| *a == b"--detach") {
                continue;
            }
            let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
                continue;
            };
            let var = |name: &str| {
                environ.split(|b| *b == 0).find_map(|pair| {
                    let pair = std::str::from_utf8(pair).ok()?;
                    pair.strip_prefix(name)?
                        .strip_prefix('=')
                        .map(str::to_owned)
                })
            };
            let (Some(runner), Some(data)) = (var(RUNNER_PID_ENV), var("WITHNAV_DATA_DIR")) else {
                continue;
            };
            let runner_alive = std::path::Path::new("/proc").join(&runner).exists();
            if runner_alive || !Path::new(&data).starts_with(&temp) {
                continue;
            }
            // Only the process is stopped: its directory is left in place, since nothing
            // records which directory a test run owns, and guessing from the temporary-directory
            // prefix could delete another run's files.
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
    }
}

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
        record["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
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
