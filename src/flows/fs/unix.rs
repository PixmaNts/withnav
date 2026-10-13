//! Descriptor-held no-follow filesystem access; paths are never reopened after validation.
use nix::{
    fcntl::{open, openat, renameat, Flock, FlockArg, OFlag},
    sys::stat::{fstat, mkdirat, Mode, SFlag},
    unistd::{getuid, unlinkat, UnlinkatFlags},
};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsFd, AsRawFd, OwnedFd},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

/// Atomic replacement changes data inodes; lock a stable sibling inode instead.
pub fn resource_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<std::collections::BTreeMap<PathBuf, Weak<Mutex<()>>>>> =
        OnceLock::new();
    let key = path
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| path.file_name().map(|n| p.join(n)))
        .unwrap_or_else(|| path.to_path_buf());
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

pub struct Directory(OwnedFd);
fn io(error: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error as i32)
}
impl Directory {
    pub fn lock(&self, name: &str) -> std::io::Result<Flock<File>> {
        valid_name(name)?;
        let fd = openat(
            &self.0,
            name,
            OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(io)?;
        check(&fd, true)?;
        if !SFlag::from_bits_truncate(fstat(&fd).map_err(io)?.st_mode).contains(SFlag::S_IFREG) {
            return Err(std::io::Error::other("lock is not a regular file"));
        }
        Flock::lock(File::from(fd), FlockArg::LockExclusive).map_err(|(_, e)| io(e))
    }
    pub fn private(path: &Path, create: bool) -> std::io::Result<Self> {
        let dir = Self::open(path, create)?;
        let stat = fstat(&dir.0).map_err(io)?;
        if stat.st_mode & 0o077 != 0 {
            return Err(std::io::Error::other(
                "private directory permissions required",
            ));
        }
        Ok(dir)
    }
    pub fn open(path: &Path, create: bool) -> std::io::Result<Self> {
        if !path.is_absolute() {
            return Err(std::io::Error::other("absolute directory required"));
        }
        let mut fd = open(
            "/",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?;
        let components: Vec<_> = path.components().collect();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                if matches!(component, Component::RootDir) {
                    continue;
                }
                return Err(std::io::Error::other("invalid directory component"));
            };
            let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
            match openat(&fd, *name, flags, Mode::empty()) {
                Ok(next) => fd = next,
                Err(nix::errno::Errno::ENOENT) if create => {
                    match mkdirat(&fd, *name, Mode::from_bits_truncate(0o700)) {
                        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                        Err(e) => return Err(io(e)),
                    }
                    fd = openat(&fd, *name, flags, Mode::empty()).map_err(io)?;
                }
                Err(e) => return Err(io(e)),
            }
            if index + 1 == components.len() {
                check(&fd, false)?;
            }
        }
        Ok(Self(fd))
    }
    pub fn read(&self, name: &str, max: usize, private: bool) -> std::io::Result<String> {
        self.read_with_mode(name, max, private, false)
    }
    /// Reload signals are user-owned regular files with no group/other access.
    pub fn read_private(&self, name: &str, max: usize) -> std::io::Result<String> {
        self.read_with_mode(name, max, true, true)
    }
    fn read_with_mode(
        &self,
        name: &str,
        max: usize,
        private: bool,
        exact: bool,
    ) -> std::io::Result<String> {
        let file = self.open_read(name, max, private, exact)?;
        let mut text = String::new();
        file.take((max as u64).saturating_add(1))
            .read_to_string(&mut text)?;
        if text.len() > max {
            return Err(std::io::Error::other("file exceeds byte limit"));
        }
        Ok(text)
    }
    pub(crate) fn open_read(
        &self,
        name: &str,
        max: usize,
        private: bool,
        exact: bool,
    ) -> std::io::Result<File> {
        valid_name(name)?;
        let fd = openat(
            &self.0,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?;
        let stat = fstat(&fd).map_err(io)?;
        if stat.st_uid != getuid().as_raw()
            || (private && stat.st_mode & 0o022 != 0)
            || (exact && stat.st_mode & 0o777 != 0o600)
        {
            return Err(std::io::Error::other("unsafe owner or permissions"));
        }
        if !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFREG) {
            return Err(std::io::Error::other("not a regular file"));
        }
        if stat.st_size < 0 || stat.st_size as u64 > max as u64 {
            return Err(std::io::Error::other("file exceeds byte limit"));
        }
        Ok(File::from(fd))
    }
    /// The caller holds the stable resource lock through append and any compaction.
    pub(crate) fn append(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        valid_name(name)?;
        let fd = openat(
            &self.0,
            name,
            OFlag::O_RDWR
                | OFlag::O_APPEND
                | OFlag::O_CREAT
                | OFlag::O_NOFOLLOW
                | OFlag::O_NONBLOCK
                | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(io)?;
        let stat = fstat(&fd).map_err(io)?;
        if stat.st_uid != getuid().as_raw()
            || stat.st_mode & 0o777 != 0o600
            || stat.st_nlink != 1
            || !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFREG)
        {
            return Err(std::io::Error::other("unsafe capture file"));
        }
        let mut file = File::from(fd);
        let length = file.metadata()?.len();
        let result = (|| {
            if length > 0 {
                file.seek(SeekFrom::End(-1))?;
                let mut last = [0];
                file.read_exact(&mut last)?;
                // Separate a crash-truncated tail so it cannot swallow the next capture.
                if last[0] != b'\n' {
                    file.write_all(b"\n")?;
                }
            }
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if result.is_err() {
            // Preserve previously accepted captures on a partial write.
            file.set_len(length)?;
            file.sync_all()?;
        }
        result
    }
    pub fn names(&self) -> std::io::Result<Vec<String>> {
        let mut dir = nix::dir::Dir::from_fd(nix::unistd::dup(&self.0).map_err(io)?).map_err(io)?;
        let mut names = Vec::new();
        for entry in dir.iter() {
            let entry = entry.map_err(io)?;
            if let Ok(name) = entry.file_name().to_str() {
                if name != "." && name != ".." {
                    names.push(name.into());
                }
            }
            if names.len() > 4096 {
                return Err(std::io::Error::other("too many directory entries"));
            }
        }
        names.sort();
        Ok(names)
    }
    pub fn write(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.install(name, bytes, false)
    }
    pub fn write_new(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.install(name, bytes, true)
    }
    fn install(&self, name: &str, bytes: &[u8], no_replace: bool) -> std::io::Result<()> {
        valid_name(name)?;
        match openat(
            &self.0,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => check(&fd, false)?,
            Err(nix::errno::Errno::ENOENT) => {}
            Err(e) => return Err(io(e)),
        }
        let temporary = format!(".flow-{}", ulid::Ulid::generate());
        let fd = openat(
            &self.0,
            temporary.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(io)?;
        let result = (|| {
            let mut file = File::from(fd);
            file.write_all(bytes)?;
            file.sync_all()?;
            if no_replace {
                // nix wraps renameat2 on glibc only, and the static musl release binary needs it:
                // measured 2026-10-12, Rust's musl libc.a and zig 0.16's musl export no renameat2
                // symbol. The raw syscall keeps the single atomic rename that fails with EEXIST
                // instead of replacing, so behaviour is the same on every libc.
                let old = CString::new(temporary.as_str())?;
                let new = CString::new(name)?;
                // SAFETY: both C strings and the owned directory fd remain live for the call.
                let result = unsafe {
                    nix::libc::syscall(
                        nix::libc::SYS_renameat2,
                        self.0.as_raw_fd(),
                        old.as_ptr(),
                        self.0.as_raw_fd(),
                        new.as_ptr(),
                        nix::libc::RENAME_NOREPLACE,
                    )
                };
                nix::errno::Errno::result(result).map(|_| ()).map_err(io)
            } else {
                renameat(&self.0, temporary.as_str(), &self.0, name).map_err(io)
            }
        })();
        if result.is_err() {
            let _ = unlinkat(&self.0, temporary.as_str(), UnlinkatFlags::NoRemoveDir);
        }
        result
    }
    pub fn remove(&self, name: &str) -> std::io::Result<()> {
        valid_name(name)?;
        unlinkat(&self.0, name, UnlinkatFlags::NoRemoveDir).map_err(io)
    }
}
fn valid_name(name: &str) -> std::io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        Err(std::io::Error::other("invalid sibling filename"))
    } else {
        Ok(())
    }
}
fn check(fd: &OwnedFd, private: bool) -> std::io::Result<()> {
    let stat = fstat(fd.as_fd()).map_err(io)?;
    if stat.st_uid != getuid().as_raw() || (private && stat.st_mode & 0o022 != 0) {
        return Err(std::io::Error::other("unsafe owner or permissions"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_read_refuses_nonprivate_modes_and_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let dir = Directory::open(temp.path(), false).unwrap();
        dir.write("marker", b"1").unwrap();
        assert_eq!(dir.read_private("marker", 10).unwrap(), "1");
        assert_eq!(dir.read_private("marker", usize::MAX).unwrap(), "1");
        assert_eq!(dir.read_private("marker", 1).unwrap(), "1");
        assert_eq!(
            dir.read_private("marker", 0).unwrap_err().to_string(),
            "file exceeds byte limit"
        );
        std::fs::set_permissions(
            temp.path().join("marker"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(dir.read_private("marker", 10).is_err());
        symlink("marker", temp.path().join("link")).unwrap();
        assert!(dir.read_private("link", 10).is_err());
        assert!(dir.open_read("link", 10, true, false).is_err());
        assert!(dir.append("link", b"2\n").is_err());
        assert!(dir.append("marker", b"2\n").is_err());
        std::fs::set_permissions(
            temp.path().join("marker"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::hard_link(temp.path().join("marker"), temp.path().join("hardlink")).unwrap();
        assert!(dir.append("hardlink", b"2\n").is_err());
        dir.append("runs", b"first\n").unwrap();
        dir.append("runs", b"second\n").unwrap();
        assert_eq!(dir.read_private("runs", 100).unwrap(), "first\nsecond\n");
    }
    #[test]
    fn no_replace_install_preserves_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = Directory::open(temp.path(), false).unwrap();
        dir.write_new("flow.toml", b"original").unwrap();
        assert_eq!(
            dir.write_new("flow.toml", b"replacement")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(dir.read("flow.toml", 100, false).unwrap(), "original");
    }
}
