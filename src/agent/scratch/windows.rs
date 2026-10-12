use std::path::{Path, PathBuf};

static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn warn_once(error: &str) {
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!("scratch directory disabled: {error}");
    }
}

pub fn prepare(_path: &Path) -> Result<PathBuf, String> {
    Err("Private scratch storage is not yet supported on Windows".into())
}

pub(crate) fn private_runtime_usable(_path: &Path) -> bool {
    false
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

impl Identity {
    pub fn capture(_path: &Path) -> Result<Self, String> {
        Err("Scratch identity capture is not yet supported on Windows".into())
    }

    pub fn open(&self) -> Result<std::fs::File, String> {
        Err("Checked scratch access is not yet supported on Windows".into())
    }

    pub fn valid(&self) -> bool {
        false
    }
}
