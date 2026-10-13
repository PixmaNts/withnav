use std::path::{Path, PathBuf};
static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn warn_once(error: &str) {
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!("scratch directory disabled: {error}");
    }
}
pub fn prepare(path: &Path) -> Result<PathBuf, String> {
    Ok(Identity::capture(path)?.path)
}
pub(crate) fn private_runtime_usable(path: &Path) -> bool {
    path.is_absolute() && path.is_dir()
}
/// Windows scratch roots use paths, not Unix device/inode bindings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub path: PathBuf,
}
impl Identity {
    pub fn capture(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("scratch directory must be absolute".into());
        }
        std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        if !path.is_dir() {
            return Err("scratch path is not a directory".into());
        }
        Ok(Self { path })
    }
    pub fn valid(&self) -> bool {
        private_runtime_usable(&self.path)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_root_is_created_and_removal_disables_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scratch");
        let identity = Identity::capture(&path).unwrap();
        assert!(identity.valid());
        std::fs::remove_dir(&path).unwrap();
        assert!(!identity.valid());
        assert!(prepare(Path::new("relative")).is_err());
    }
}
