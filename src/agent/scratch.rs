//! Private shared temporary storage. Never trust a pre-existing shared-temp link.
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn warn_once(error: &str) {
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!("scratch directory disabled: {error}");
    }
}

pub fn prepare(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("scratch directory must be absolute".into());
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|e| e.to_string())?;
        }
        Err(error) => return Err(error.to_string()),
        Ok(_) => {}
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    let uid = nix::unistd::Uid::current().as_raw();
    if !acceptable(&metadata, uid) {
        return Err("scratch directory must be a real directory owned by the current user".into());
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    path.canonicalize().map_err(|e| e.to_string())
}

fn acceptable(metadata: &std::fs::Metadata, uid: u32) -> bool {
    metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == uid
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scratch_is_private_and_links_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scratch");
        assert_eq!(prepare(&path).unwrap(), path);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(path, &link).unwrap();
        assert!(prepare(&link).is_err());
        let metadata = std::fs::metadata(temp.path()).unwrap();
        assert!(!acceptable(&metadata, metadata.uid().wrapping_add(1)));
    }
}
