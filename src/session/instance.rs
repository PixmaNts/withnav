use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use nix::fcntl::{Flock, FlockArg};
use ulid::Ulid;

#[cfg(unix)]
type OwnerLock = Flock<File>;
#[cfg(windows)]
type OwnerLock = File;

#[cfg(unix)]
fn probe(file: File) -> Result<Option<OwnerLock>, String> {
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, error)) => Err(error.to_string()),
    }
}

#[cfg(windows)]
fn probe(file: File) -> Result<Option<OwnerLock>, String> {
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.to_string()),
    }
}

#[derive(Debug)]
pub(super) struct Instance {
    pub id: Ulid,
    pub directory: PathBuf,
    _lock: OwnerLock,
}

impl Instance {
    pub fn acquire(history_db: &Path) -> Result<Self, String> {
        let database = std::fs::canonicalize(history_db).map_err(|e| e.to_string())?;
        let mut directory = database.into_os_string();
        directory.push(".instances");
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let id = Ulid::generate();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.join(format!("{id}.lock")))
            .map_err(|e| e.to_string())?;
        let lock = probe(file)?.ok_or_else(|| "instance lock is already held".to_string())?;
        Ok(Self {
            id,
            directory,
            _lock: lock,
        })
    }
}

/// Keep the stale owner's lock acquired through recovery. Failure to probe
/// ownership is conservative: a possibly live instance is never recovered.
pub(super) fn stale_lock(directory: &Path, owner: Ulid) -> Result<Option<OwnerLock>, String> {
    let path = directory.join(format!("{owner}.lock"));
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => return Err(error.to_string()),
    };
    probe(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_held_until_instance_drops() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("history.db");
        std::fs::write(&database, []).unwrap();
        let instance = Instance::acquire(&database).unwrap();
        let directory = instance.directory.clone();
        let owner = instance.id;
        assert!(stale_lock(&directory, owner).unwrap().is_none());
        assert!(directory.join(format!("{owner}.lock")).exists());
        drop(instance);
        let recovery = stale_lock(&directory, owner).unwrap().unwrap();
        assert!(stale_lock(&directory, owner).unwrap().is_none());
        drop(recovery);
        assert!(stale_lock(&directory, owner).unwrap().is_some());
    }
}
