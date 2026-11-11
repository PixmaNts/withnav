//! `<data dir>/credentials/secrets.json`: the workspace secrets that outlive a conversation.
//! Written whole and atomically (a private temporary file renamed over it), mode 0600 in a
//! 0700 directory (on Windows, a protected DACL for the current user, SYSTEM and Administrators
//! on both); removed when it would be empty. Never read or written past `MAX_FILE_BYTES`.
//! A save returns once the replacement is durable: the file is synced before the rename and the
//! directory after it (on Windows the rename is a write-through move), so a power cut cannot bring
//! the previous content back.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::SecretTtl;

const VERSION: u32 = 1;

/// The largest secrets file read or written: about 250 values at the 16 KiB value cap. A larger
/// file is refused before its content is read, so a corrupted or planted file cannot make the
/// server allocate its whole size at start.
pub(super) const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub(super) struct Saved {
    pub name: String,
    pub value: String,
    pub workspace: PathBuf,
    pub ttl: SecretTtl,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(default)]
    pub use_count: u64,
}

#[derive(Serialize, Deserialize)]
struct FileBody {
    version: u32,
    secrets: Vec<Saved>,
}

/// The saved secrets; none when there is no file.
pub(super) fn load(path: &Path) -> std::io::Result<Vec<Saved>> {
    // Tightened before the content is read: a permissive file is not left exposed while it is
    // loaded, and a file that cannot be tightened is refused (nothing is loaded from it).
    match restrict(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        Ok(()) => {}
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let size = file.metadata()?.len();
    if size > MAX_FILE_BYTES {
        return Err(too_large(size));
    }
    let bytes = read_bounded(file, MAX_FILE_BYTES)?;
    let body: FileBody = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    if body.version != VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unknown secrets file version {}", body.version),
        ));
    }
    Ok(body.secrets)
}

/// At most `max` bytes of `reader`; an error past that, without reading further (the file may
/// have grown since its size was checked).
pub(super) fn read_bounded(reader: impl Read, max: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(too_large(bytes.len() as u64));
    }
    Ok(bytes)
}

fn too_large(size: u64) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("secrets file holds {size} bytes or more, past the {MAX_FILE_BYTES} byte limit"),
    )
}

/// Replaces the file with `secrets`, or removes it when there are none.
pub(super) fn save(path: &Path, secrets: &[Saved]) -> std::io::Result<()> {
    if secrets.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => sync_parent(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("secrets file has no directory"))?;
    let dir_existed = dir.is_dir();
    create_private_dir(dir)?;
    let body = serde_json::to_vec(&FileBody {
        version: VERSION,
        secrets: secrets
            .iter()
            .map(|s| Saved {
                name: s.name.clone(),
                value: s.value.clone(),
                workspace: s.workspace.clone(),
                ttl: s.ttl,
                created_at: s.created_at,
                expires_at: s.expires_at,
                use_count: s.use_count,
            })
            .collect(),
    })
    .map_err(std::io::Error::other)?;
    if body.len() as u64 > MAX_FILE_BYTES {
        // Written, the next start would refuse the file and lose every secret in it.
        return Err(too_large(body.len() as u64));
    }
    // `tempfile` creates the file 0600 on Unix: the value is never readable by others, even
    // before the rename.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    // On Windows the temporary file inherits the directory's ACEs: it is made private before the
    // value is written into it.
    #[cfg(windows)]
    protect(tmp.path())?;
    tmp.write_all(&body)?;
    tmp.as_file().sync_all()?;
    replace(tmp, path)?;
    restrict(path)?;
    if !dir_existed {
        // A directory this call created is itself an entry of its parent.
        sync_parent(dir)?;
    }
    sync_parent(path)
}

/// Renames the synced temporary file over `path`.
#[cfg(not(windows))]
fn replace(tmp: tempfile::NamedTempFile, path: &Path) -> std::io::Result<()> {
    tmp.persist(path).map(drop).map_err(|e| e.error)
}

/// Renames the synced temporary file over `path`, returning once the move reached the disk.
#[cfg(windows)]
fn replace(tmp: tempfile::NamedTempFile, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    // Closes the handle, and removes the file again if the move fails.
    let mut tmp = tmp.into_temp_path();
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() };
    let (from, to) = (wide(&tmp), wide(path));
    // SAFETY: both buffers are NUL-terminated and live across the call.
    let moved = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The temporary name is gone: nothing is left for the guard to delete. `keep` would touch
    // that name again, so cleanup is only switched off.
    tmp.disable_cleanup(true);
    Ok(())
}

/// Makes a rename or removal in `path`'s directory durable.
#[cfg(unix)]
fn sync_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => std::fs::File::open(dir)?.sync_all(),
        _ => Ok(()),
    }
}

/// Directories cannot be opened for syncing here; a removal is not written through.
#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    protect(dir)
}

#[cfg(not(any(unix, windows)))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Owner read and write only.
#[cfg(unix)]
fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// The file and its directory: owner, SYSTEM and Administrators only. Also run on load, so a
/// directory created before this protection gets it at the next start.
#[cfg(windows)]
fn restrict(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        protect(dir)?;
    }
    protect(path)
}

#[cfg(windows)]
fn protect(path: &Path) -> std::io::Result<()> {
    withnav_provider::auth::protect_private(path).map_err(|error| {
        std::io::Error::other(format!("cannot restrict {}: {error}", path.display()))
    })
}

#[cfg(not(any(unix, windows)))]
fn restrict(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
