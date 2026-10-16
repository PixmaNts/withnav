//! Private shared temporary storage. Never trust a pre-existing shared-temp link.
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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

fn open_checked(path: &Path, create: bool) -> Result<std::fs::File, String> {
    use nix::fcntl::{AT_FDCWD, OFlag, openat};
    use nix::sys::stat::{Mode, mkdirat};
    use std::os::fd::AsFd;
    let uid = nix::unistd::Uid::current().as_raw();
    if !path.is_absolute() {
        return Err("scratch directory must be absolute".into());
    }
    let mut file = std::fs::File::from(
        openat(
            AT_FDCWD,
            "/",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| e.to_string())?,
    );
    let components: Vec<_> = path
        .components()
        .filter(|c| !matches!(c, std::path::Component::RootDir))
        .collect();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            return Err("invalid scratch component".into());
        };
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if (metadata.uid() != 0 && metadata.uid() != uid)
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return Err("unsafe scratch ancestor".into());
        }
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let fd = match openat(file.as_fd(), *name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(nix::errno::Errno::ENOENT) if create => {
                mkdirat(file.as_fd(), *name, Mode::from_bits_truncate(0o700))
                    .map_err(|e| e.to_string())?;
                openat(file.as_fd(), *name, flags, Mode::empty()).map_err(|e| e.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
        file = std::fs::File::from(fd);
        if index + 1 == components.len()
            && !acceptable(&file.metadata().map_err(|e| e.to_string())?, uid)
        {
            return Err("scratch directory must be owned by current user".into());
        }
    }
    Ok(file)
}

pub(crate) fn private_runtime_usable(path: &Path) -> bool {
    open_checked(path, false).is_ok_and(|file| {
        file.metadata()
            .is_ok_and(|metadata| metadata.mode() & 0o777 == 0o700)
    })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}
impl Identity {
    pub fn capture(path: &Path) -> Result<Self, String> {
        let normalized: PathBuf = path.components().collect();
        let file = open_checked(&normalized, true)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        #[cfg(test)]
        PREPARE_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let path = normalized.canonicalize().map_err(|e| e.to_string())?;
        let now = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        let opened = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .map_err(|e| e.to_string())?;
        if metadata.dev() != now.dev() || metadata.ino() != now.ino() || opened != path {
            return Err("scratch directory changed during preparation".into());
        }
        Ok(Self {
            path,
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    pub fn open(&self) -> Result<std::fs::File, String> {
        let file = open_checked(&self.path, false)?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if metadata.dev() != self.dev
            || metadata.ino() != self.ino
            || metadata.mode() & 0o777 != 0o700
        {
            return Err("scratch directory identity changed".into());
        }
        Ok(file)
    }
    pub fn valid(&self) -> bool {
        self.open().is_ok()
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
    fn validation_refuses_widened_permissions_without_repair() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scratch");
        let identity = Identity::capture(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!identity.valid());
        assert!(
            crate::tools::nofollow::write_scratch(&path.join("file"), b"x", &identity).is_err()
        );
        assert!(
            crate::tools::nofollow::edit_scratch(&path.join("file"), &identity, |_| Ok((
                "x".into(),
                "edited".into()
            )))
            .is_err()
        );
        assert!(!path.join("file").exists());
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o777);
    }
    #[test]
    fn trailing_separator_and_ancestor_links_never_change_target_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(Identity::capture(&PathBuf::from(format!("{}/", link.display()))).is_err());
        assert!(Identity::capture(&link.join("child")).is_err());
        assert_eq!(std::fs::metadata(target).unwrap().mode() & 0o777, 0o755);
    }
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
