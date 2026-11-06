//! `<data dir>/credentials/secrets.json`: the workspace secrets that outlive a conversation.
//! Written whole and atomically (a private temporary file renamed over it), mode 0600 in a
//! 0700 directory; removed when it would be empty.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::SecretTtl;

const VERSION: u32 = 1;

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
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    restrict(path)?;
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

/// Replaces the file with `secrets`, or removes it when there are none.
pub(super) fn save(path: &Path, secrets: &[Saved]) -> std::io::Result<()> {
    if secrets.is_empty() {
        return match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("secrets file has no directory"))?;
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
    // `tempfile` creates the file 0600 on Unix: the value is never readable by others, even
    // before the rename.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&body)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    restrict(path)
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

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Owner read and write only.
#[cfg(unix)]
fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
