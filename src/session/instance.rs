use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use ulid::Ulid;

#[derive(Debug)]
pub(super) struct Instance {
    pub id: Ulid,
    pub directory: PathBuf,
    _lock: Flock<File>,
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
        let lock =
            Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| e.to_string())?;
        Ok(Self {
            id,
            directory,
            _lock: lock,
        })
    }
}

/// Keep the stale owner's lock acquired through recovery. Failure to probe
/// ownership is conservative: a possibly live instance is never recovered.
pub(super) fn stale_lock(directory: &Path, owner: Ulid) -> Result<Option<Flock<File>>, String> {
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
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, error)) => Err(error.to_string()),
    }
}
