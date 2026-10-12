use std::path::{Path, PathBuf};
use ulid::Ulid;

#[derive(Debug)]
pub(crate) struct Instance {
    pub id: Ulid,
    pub directory: PathBuf,
}

impl Instance {
    pub fn acquire(_history_db: &Path) -> Result<Self, String> {
        Err("Session ownership locking is not yet supported on Windows".into())
    }
}

pub(crate) fn stale_lock(_directory: &Path, _owner: Ulid) -> Result<Option<()>, String> {
    Err("Session ownership recovery is not yet supported on Windows".into())
}
