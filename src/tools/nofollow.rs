//! Opening the file of a `write` or `edit` WITHOUT following links, for a call
//! whose destination was reviewed.
//!
//! A reviewed destination is a canonical path: no link in it when it was
//! checked. Handing that path to the filesystem again would resolve it again,
//! and a link put in its place in between would redirect the write. So the
//! bound path is walked here with `O_NOFOLLOW` on every component (and with
//! `openat2` + `RESOLVE_NO_SYMLINKS` first, when the kernel has it), the
//! parent directory is held by descriptor, and the file is read, created and
//! renamed relative to that descriptor. A link anywhere is
//! [`ToolError::DestinationChanged`], never a silent follow: the caller
//! checks the call again.
//!
//! The write is atomic as before (a temp file beside the target, then a
//! rename). A rename replaces the directory entry and never follows it, and
//! the entry is checked to be a regular file (or absent) right before.

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};

use nix::errno::Errno;
use nix::fcntl::{openat, renameat, OFlag, AT_FDCWD};
use nix::sys::stat::{fstatat, mkdirat, Mode, SFlag};
use nix::unistd::{unlinkat, UnlinkatFlags};

use crate::error::ToolError;

#[cfg(test)]
thread_local! {
    static BEFORE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Test seam: `hook` runs once, right before the next bound open, so a test
/// can move a file after every earlier check and before the open.
#[cfg(test)]
pub(crate) fn on_next_open(hook: impl FnOnce() + 'static) {
    BEFORE_OPEN.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
}

fn before_open() {
    #[cfg(test)]
    if let Some(hook) = BEFORE_OPEN.with(|h| h.borrow_mut().take()) {
        hook();
    }
}

fn changed(path: &Path) -> ToolError {
    ToolError::DestinationChanged(path.to_path_buf())
}

/// Holds the reviewed read target without following a replacement link.
pub fn open_read(destination: &Path) -> Result<std::fs::File, ToolError> {
    before_open();
    if destination == Path::new("/") {
        return open_dir(destination, false, true).map(std::fs::File::from);
    }
    let (parent, name) = split(destination)?;
    let dir = open_dir(parent, false, true)?;
    let fd = openat(
        dir.as_fd(),
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| walk_error(error, destination))?;
    let file = std::fs::File::from(fd);
    let metadata = file.metadata()?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(changed(destination));
    }
    Ok(file)
}

#[cfg(test)]
#[test]
fn replaced_or_missing_scratch_root_is_not_recreated_at_native_open() {
    for remove in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("scratch");
        let identity = crate::agent::scratch::Identity::capture(&root).unwrap();
        on_next_open({
            let root = root.clone();
            let old = temp.path().join("old");
            move || {
                std::fs::rename(&root, old).unwrap();
                if !remove {
                    std::fs::create_dir(root).unwrap();
                }
            }
        });
        assert!(matches!(
            write_scratch(&root.join("x"), b"new", &identity),
            Err(ToolError::DestinationChanged(_))
        ));
        assert!(!root.join("x").exists());
        assert_eq!(root.exists(), !remove);
    }
}

#[cfg(test)]
#[test]
fn missing_scratch_edit_never_creates_parent_directories() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("scratch");
    let identity = crate::agent::scratch::Identity::capture(&root).unwrap();
    for target in [root.join("missing/deeper/file"), root.join("absent")] {
        assert!(matches!(
            edit_scratch(&target, &identity, |_| panic!(
                "missing edit must not call change"
            )),
            Err(ToolError::NotFound(_))
        ));
    }
    assert!(!root.join("missing").exists());
}

/// What an errno during a no-follow walk means: a link (or something that is
/// not a directory) where the reviewed path had a directory or file, or a
/// path that is gone: all of them "the destination changed". The one
/// exception is an `edit` whose final file is gone: it is reported as not
/// found, like any edit of a missing file, and nothing is written.
fn walk_error(errno: Errno, path: &Path) -> ToolError {
    match errno {
        Errno::ELOOP
        | Errno::ENOTDIR
        | Errno::EXDEV
        | Errno::EISDIR
        | Errno::ENOENT
        | Errno::EMLINK => changed(path),
        other => ToolError::Io(std::io::Error::from(other)),
    }
}

const DIR_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_CLOEXEC);

/// `path`, an absolute canonical folder, opened with no link anywhere in it.
/// With `create`, the missing tail is made one folder at a time, each opened
/// without following.
fn open_dir(path: &Path, create: bool, use_openat2: bool) -> Result<OwnedFd, ToolError> {
    #[cfg(target_os = "linux")]
    if use_openat2 {
        use nix::fcntl::{openat2, OpenHow, ResolveFlag};
        let how = OpenHow::new()
            .flags(DIR_FLAGS)
            .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS);
        match openat2(AT_FDCWD, path, how) {
            Ok(fd) => return Ok(fd),
            // Not there yet: the walk below can create it.
            Err(Errno::ENOENT) if create => {}
            // A kernel, or a sandbox, without openat2: the walk is the same
            // guarantee, component by component.
            Err(Errno::ENOSYS | Errno::EPERM | Errno::EINVAL | Errno::E2BIG) => {}
            Err(errno) => return Err(walk_error(errno, path)),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = use_openat2;
    walk_dir(path, create)
}

fn walk_dir(path: &Path, create: bool) -> Result<OwnedFd, ToolError> {
    let mut dir = openat(AT_FDCWD, "/", DIR_FLAGS | OFlag::O_NOFOLLOW, Mode::empty())
        .map_err(|e| walk_error(e, path))?;
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            _ => return Err(changed(path)),
        };
        let flags = DIR_FLAGS | OFlag::O_NOFOLLOW;
        dir = match openat(dir.as_fd(), name, flags, Mode::empty()) {
            Ok(next) => next,
            Err(Errno::ENOENT) if create => {
                match mkdirat(dir.as_fd(), name, Mode::from_bits_truncate(0o777)) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(errno) => return Err(walk_error(errno, path)),
                }
                openat(dir.as_fd(), name, flags, Mode::empty()).map_err(|e| walk_error(e, path))?
            }
            Err(errno) => return Err(walk_error(errno, path)),
        };
    }
    Ok(dir)
}

fn split(destination: &Path) -> Result<(&Path, &std::ffi::OsStr), ToolError> {
    match (destination.parent(), destination.file_name()) {
        (Some(parent), Some(name)) if destination.is_absolute() => Ok((parent, name)),
        _ => Err(changed(destination)),
    }
}

/// Creates or replaces `destination`, a canonical file path, atomically and
/// without following a link anywhere in it.
pub fn write_atomic(destination: &Path, content: &[u8]) -> Result<(), ToolError> {
    write_atomic_with(destination, content, true)
}

fn scratch_parent(
    destination: &Path,
    identity: &crate::agent::scratch::Identity,
    create: bool,
) -> Result<(OwnedFd, std::ffi::OsString), ToolError> {
    before_open();
    let root = identity.open().map_err(|_| changed(destination))?;
    let relative = destination
        .strip_prefix(&identity.path)
        .map_err(|_| changed(destination))?;
    let (parent, name) = (
        relative.parent().ok_or_else(|| changed(destination))?,
        relative.file_name().ok_or_else(|| changed(destination))?,
    );
    let mut dir: OwnedFd = root.into();
    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(changed(destination));
        };
        dir = match openat(
            dir.as_fd(),
            name,
            DIR_FLAGS | OFlag::O_NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) if create => {
                mkdirat(dir.as_fd(), name, Mode::from_bits_truncate(0o700))
                    .map_err(|e| walk_error(e, destination))?;
                openat(
                    dir.as_fd(),
                    name,
                    DIR_FLAGS | OFlag::O_NOFOLLOW,
                    Mode::empty(),
                )
                .map_err(|e| walk_error(e, destination))?
            }
            Err(Errno::ENOENT) => return Err(ToolError::NotFound(destination.to_path_buf())),
            Err(error) => return Err(walk_error(error, destination)),
        };
    }
    Ok((dir, name.to_os_string()))
}

pub fn write_scratch(
    destination: &Path,
    content: &[u8],
    identity: &crate::agent::scratch::Identity,
) -> Result<(), ToolError> {
    let (dir, name) = scratch_parent(destination, identity, true)?;
    replace_in(&dir, &name, content, destination)
}

pub fn edit_scratch(
    destination: &Path,
    identity: &crate::agent::scratch::Identity,
    change: impl FnOnce(String) -> Result<(String, String), ToolError>,
) -> Result<String, ToolError> {
    let (dir, name) = scratch_parent(destination, identity, false)?;
    let fd = openat(
        dir.as_fd(),
        name.as_os_str(),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| {
        if e == Errno::ENOENT {
            ToolError::NotFound(destination.to_path_buf())
        } else {
            walk_error(e, destination)
        }
    })?;
    let mut file = std::fs::File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(changed(destination));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let (updated, report) = change(text)?;
    replace_in(&dir, &name, updated.as_bytes(), destination)?;
    Ok(report)
}

fn write_atomic_with(
    destination: &Path,
    content: &[u8],
    use_openat2: bool,
) -> Result<(), ToolError> {
    before_open();
    let (parent, name) = split(destination)?;
    let dir = open_dir(parent, true, use_openat2)?;
    replace_in(&dir, name, content, destination)
}

/// Reads `destination` through a no-follow descriptor, hands the text to
/// `change`, and writes what it returns back the same way. `change` may refuse
/// with a tool error, then nothing is written.
pub fn edit_atomic(
    destination: &Path,
    change: impl FnOnce(String) -> Result<(String, String), ToolError>,
) -> Result<String, ToolError> {
    edit_atomic_with(destination, change, true)
}

fn edit_atomic_with(
    destination: &Path,
    change: impl FnOnce(String) -> Result<(String, String), ToolError>,
    use_openat2: bool,
) -> Result<String, ToolError> {
    before_open();
    let (parent, name) = split(destination)?;
    let dir = open_dir(parent, false, use_openat2)?;
    // Non-blocking, so a pipe put in its place cannot hang the open; the
    // descriptor is then checked to be a regular file.
    let fd = match openat(
        dir.as_fd(),
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Err(ToolError::NotFound(destination.to_path_buf())),
        Err(errno) => return Err(walk_error(errno, destination)),
    };
    let mut file = std::fs::File::from(fd);
    if !file.metadata()?.file_type().is_file() {
        return Err(changed(destination));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let (updated, report) = change(text)?;
    replace_in(&dir, name, updated.as_bytes(), destination)?;
    Ok(report)
}

/// Writes `content` as `name` inside `dir`: temp file, sync, rename. The
/// existing entry must be a regular file or absent.
fn replace_in(
    dir: &OwnedFd,
    name: &std::ffi::OsStr,
    content: &[u8],
    destination: &Path,
) -> Result<(), ToolError> {
    let temp = format!(".navai-{}.tmp", ulid::Ulid::new());
    let fd = openat(
        dir.as_fd(),
        temp.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|e| walk_error(e, destination))?;
    let cleanup = |dir: &OwnedFd| {
        let _ = unlinkat(dir.as_fd(), temp.as_str(), UnlinkatFlags::NoRemoveDir);
    };
    let mut file = std::fs::File::from(fd);
    let written = file.write_all(content).and_then(|()| file.sync_all());
    if let Err(e) = written {
        cleanup(dir);
        return Err(e.into());
    }
    drop(file);
    // The entry about to be replaced must be what was reviewed: a file.
    match fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFREG => {}
        Ok(_) => {
            cleanup(dir);
            return Err(changed(destination));
        }
        Err(Errno::ENOENT) => {}
        Err(errno) => {
            cleanup(dir);
            return Err(walk_error(errno, destination));
        }
    }
    if let Err(errno) = renameat(dir.as_fd(), temp.as_str(), dir.as_fd(), name) {
        cleanup(dir);
        return Err(walk_error(errno, destination));
    }
    if let Ok(copy) = dir.try_clone() {
        let _ = std::fs::File::from(copy).sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn ws() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        (tmp, root)
    }

    fn is_changed<T: std::fmt::Debug>(result: Result<T, ToolError>) -> bool {
        matches!(result, Err(ToolError::DestinationChanged(_)))
    }

    /// Every case runs with openat2 and with the component walk alone.
    const BOTH: [bool; 2] = [true, false];

    #[test]
    fn write_creates_replaces_and_makes_missing_folders() {
        for use_openat2 in BOTH {
            let (_tmp, root) = ws();
            let file = root.join("a/b/c.txt");
            write_atomic_with(&file, b"one", use_openat2).unwrap();
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "one");
            write_atomic_with(&file, b"two", use_openat2).unwrap();
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
            let names: Vec<_> = std::fs::read_dir(root.join("a/b"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, ["c.txt"], "no temp file is left");
        }
    }

    #[test]
    fn a_link_in_the_final_component_is_never_followed() {
        for use_openat2 in BOTH {
            let (_tmp, root) = ws();
            let (target, link) = (root.join("main.rs"), root.join("notes.md"));
            std::fs::write(&target, "fn main() {}").unwrap();
            symlink(&target, &link).unwrap();
            assert!(is_changed(write_atomic_with(&link, b"x", use_openat2)));
            assert!(is_changed(edit_atomic_with(
                &link,
                |t| Ok((t, String::new())),
                use_openat2
            )));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "fn main() {}");
            assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
            // A dangling link is no different.
            let dangling = root.join("gone.md");
            symlink(root.join("missing"), &dangling).unwrap();
            assert!(is_changed(write_atomic_with(&dangling, b"x", use_openat2)));
            assert!(!root.join("missing").exists());
        }
    }

    #[test]
    fn a_link_in_a_parent_folder_is_never_followed() {
        for use_openat2 in BOTH {
            let (_tmp, root) = ws();
            let (real, other) = (root.join("real"), root.join("other"));
            std::fs::create_dir_all(&real).unwrap();
            std::fs::create_dir_all(&other).unwrap();
            std::fs::write(real.join("f.txt"), "keep").unwrap();
            symlink(&other, root.join("link")).unwrap();
            let through = root.join("link/f.txt");
            assert!(is_changed(write_atomic_with(&through, b"x", use_openat2)));
            assert!(is_changed(edit_atomic_with(
                &through,
                |t| Ok((t, String::new())),
                use_openat2
            )));
            // Making folders under a link redirects nothing either.
            assert!(is_changed(write_atomic_with(
                &root.join("link/new/f.txt"),
                b"x",
                use_openat2
            )));
            assert_eq!(std::fs::read_dir(&other).unwrap().count(), 0);
            assert_eq!(std::fs::read_to_string(real.join("f.txt")).unwrap(), "keep");
        }
    }

    #[test]
    fn a_folder_or_a_pipe_in_the_place_of_the_file_is_refused() {
        for use_openat2 in BOTH {
            let (_tmp, root) = ws();
            std::fs::create_dir(root.join("dir.md")).unwrap();
            assert!(is_changed(write_atomic_with(
                &root.join("dir.md"),
                b"x",
                use_openat2
            )));
            let pipe = root.join("pipe.md");
            if std::process::Command::new("mkfifo")
                .arg(&pipe)
                .status()
                .is_ok_and(|s| s.success())
            {
                assert!(is_changed(write_atomic_with(&pipe, b"x", use_openat2)));
                assert!(is_changed(edit_atomic_with(
                    &pipe,
                    |t| Ok((t, String::new())),
                    use_openat2
                )));
            }
        }
    }

    #[test]
    fn edit_reads_and_writes_back_and_a_refusal_writes_nothing() {
        for use_openat2 in BOTH {
            let (_tmp, root) = ws();
            let file = root.join("a.txt");
            std::fs::write(&file, "hello world").unwrap();
            let report = edit_atomic_with(
                &file,
                |text| Ok((text.replace("world", "there"), "1".to_string())),
                use_openat2,
            )
            .unwrap();
            assert_eq!(report, "1");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello there");
            let refused = edit_atomic_with(&file, |_| Err(ToolError::NoMatch), use_openat2);
            assert!(matches!(refused, Err(ToolError::NoMatch)));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello there");
            assert!(matches!(
                edit_atomic_with(
                    &root.join("absent.txt"),
                    |t| Ok((t, String::new())),
                    use_openat2
                ),
                Err(ToolError::NotFound(_))
            ));
        }
    }
}
