#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};
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
