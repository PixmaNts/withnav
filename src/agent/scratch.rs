//! Private shared temporary storage. Never trust a pre-existing shared-temp link.
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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
    validate_ancestors(path)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY)
        .open(path)
        .map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    let uid = nix::unistd::Uid::current().as_raw();
    if !acceptable(&metadata, uid) {
        return Err("scratch directory must be a real directory owned by the current user".into());
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        file.set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    #[cfg(test)]
    PREPARE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let canonical = path.canonicalize().map_err(|e| e.to_string())?;
    let now = std::fs::metadata(&canonical).map_err(|e| e.to_string())?;
    let opened = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(|e| e.to_string())?;
    if metadata.dev() != now.dev() || metadata.ino() != now.ino() || opened != canonical {
        return Err("scratch directory changed during preparation".into());
    }
    Ok(canonical)
}

fn validate_ancestors(path: &Path) -> Result<(), String> {
    let uid = nix::unistd::Uid::current().as_raw();
    for ancestor in path.ancestors().skip(1) {
        let metadata = std::fs::symlink_metadata(ancestor).map_err(|e| e.to_string())?;
        if !metadata.is_dir()
            || (metadata.uid() != 0 && metadata.uid() != uid)
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return Err("unsafe scratch ancestor".into());
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Identity {
    pub path: PathBuf,
    dev: u64,
    ino: u64,
}
impl Identity {
    pub fn capture(path: &Path) -> Result<Self, String> {
        let path = prepare(path)?;
        let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        Ok(Self {
            path,
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    pub fn valid(&self) -> bool {
        prepare(&self.path).is_ok()
            && std::fs::metadata(&self.path)
                .is_ok_and(|m| m.dev() == self.dev && m.ino() == self.ino)
    }
}

#[cfg(test)]
thread_local! { static PREPARE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }

fn acceptable(metadata: &std::fs::Metadata, uid: u32) -> bool {
    metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == uid
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preparation_swap_and_unsafe_ancestors_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scratch");
        let other = temp.path().join("other");
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(&other).unwrap();
        PREPARE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new({
                let path = path.clone();
                let other = other.clone();
                move || {
                    std::fs::remove_dir(&path).unwrap();
                    std::os::unix::fs::symlink(other, path).unwrap();
                }
            }))
        });
        assert!(prepare(&path).is_err());
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(prepare(&other).is_err());
    }
    #[test]
    fn replacement_or_removal_loses_scratch_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scratch");
        let identity = Identity::capture(&path).unwrap();
        std::fs::rename(&path, temp.path().join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(!identity.valid());
        std::fs::remove_dir(&path).unwrap();
        assert!(!identity.valid());
    }
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
