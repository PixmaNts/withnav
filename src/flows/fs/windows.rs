use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
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
pub struct Directory(PathBuf);
impl Directory {
    pub fn open(path: &Path, create: bool) -> std::io::Result<Self> {
        if !path.is_absolute() {
            return Err(std::io::Error::other("absolute directory required"));
        }
        if create {
            std::fs::create_dir_all(path)?;
        }
        if !path.is_dir() {
            return Err(std::io::Error::other("not a directory"));
        }
        Ok(Self(path.to_path_buf()))
    }
    pub fn private(path: &Path, create: bool) -> std::io::Result<Self> {
        Self::open(path, create)
    }
    pub fn lock(&self, name: &str) -> std::io::Result<File> {
        valid_name(name)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.0.join(name))?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("lock is not a regular file"));
        }
        file.lock()?;
        Ok(file)
    }
    pub(crate) fn open_read(
        &self,
        name: &str,
        max: usize,
        _private: bool,
        _exact: bool,
    ) -> std::io::Result<File> {
        valid_name(name)?;
        let file = File::open(self.0.join(name))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        if metadata.len() > max as u64 {
            return Err(std::io::Error::other("file exceeds byte limit"));
        }
        Ok(file)
    }
    pub fn read(&self, name: &str, max: usize, private: bool) -> std::io::Result<String> {
        let mut text = String::new();
        self.open_read(name, max, private, false)?
            .take((max as u64).saturating_add(1))
            .read_to_string(&mut text)?;
        if text.len() > max {
            return Err(std::io::Error::other("file exceeds byte limit"));
        }
        Ok(text)
    }
    pub fn read_private(&self, name: &str, max: usize) -> std::io::Result<String> {
        self.read(name, max, true)
    }
    pub(crate) fn append(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        valid_name(name)?;
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(self.0.join(name))?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("unsafe capture file"));
        }
        let length = file.metadata()?.len();
        let result = (|| {
            if length > 0 {
                file.seek(SeekFrom::End(-1))?;
                let mut last = [0];
                file.read_exact(&mut last)?;
                if last[0] != b'\n' {
                    file.write_all(b"\n")?;
                }
            }
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if result.is_err() {
            file.set_len(length)?;
            file.sync_all()?;
        }
        result
    }
    pub fn names(&self) -> std::io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.0)? {
            if let Some(name) = entry?.file_name().to_str() {
                names.push(name.to_owned());
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
        let mut temporary = tempfile::NamedTempFile::new_in(&self.0)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        let destination = self.0.join(name);
        if no_replace {
            temporary.persist_noclobber(destination)
        } else {
            temporary.persist(destination)
        }
        .map(|_| ())
        .map_err(|e| e.error)
    }
    pub fn remove(&self, name: &str) -> std::io::Result<()> {
        valid_name(name)?;
        std::fs::remove_file(self.0.join(name))
    }
}
fn valid_name(name: &str) -> std::io::Result<()> {
    let stem = name.split('.').next().unwrap_or_default().to_uppercase();
    let device = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|s| {
            matches!(
                s,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', ':', '\0'])
        || name.ends_with(['.', ' '])
        || matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        || device
    {
        Err(std::io::Error::other("invalid sibling filename"))
    } else {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sibling_names_reject_windows_aliases() {
        for name in [
            "", ".", "..", "a/b", "a\\b", "a:b", "NUL", "con.txt", "COM1", "lpt9.log", "a.", "a ",
        ] {
            assert!(valid_name(name).is_err(), "{name}");
        }
        for name in ["flows.json", "é space.txt", "COM10"] {
            assert!(valid_name(name).is_ok(), "{name}");
        }
    }
    #[test]
    fn atomic_install_and_bounded_append() {
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
        dir.write("flow.toml", b"new").unwrap();
        assert!(dir.read("flow.toml", 2, false).is_err());
        dir.append("runs", b"first").unwrap();
        dir.append("runs", b"second\n").unwrap();
        assert_eq!(dir.read_private("runs", 100).unwrap(), "first\nsecond\n");
    }
}
