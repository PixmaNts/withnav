//! Descriptor-held no-follow filesystem access; paths are never reopened after validation.
use nix::{
    fcntl::{open, openat, renameat, OFlag},
    sys::stat::{fstat, mkdirat, Mode, SFlag},
    unistd::{getuid, unlinkat, UnlinkatFlags},
};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsFd, OwnedFd},
    path::{Component, Path},
};

pub struct Directory(OwnedFd);
fn io(error: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error as i32)
}
impl Directory {
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
                    mkdirat(&fd, *name, Mode::from_bits_truncate(0o700)).map_err(io)?;
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
        valid_name(name)?;
        let fd = openat(
            &self.0,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?;
        check(&fd, private)?;
        let stat = fstat(&fd).map_err(io)?;
        if !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFREG) {
            return Err(std::io::Error::other("not a regular file"));
        }
        let mut text = String::new();
        File::from(fd)
            .take((max + 1) as u64)
            .read_to_string(&mut text)?;
        if text.len() > max {
            return Err(std::io::Error::other("file exceeds byte limit"));
        }
        Ok(text)
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
        valid_name(name)?;
        if let Ok(fd) = openat(
            &self.0,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            check(&fd, false)?;
        }
        let temporary = format!(".flow-{}", ulid::Ulid::new());
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
            renameat(&self.0, temporary.as_str(), &self.0, name).map_err(io)
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
