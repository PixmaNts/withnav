use std::{
    fs::File,
    path::Path,
    sync::{Arc, Mutex},
};

fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Checked flow storage is not yet supported on Windows",
    )
}

pub fn resource_lock(_path: &Path) -> Arc<Mutex<()>> {
    // This process-local guard cannot authorize I/O: every storage operation refuses it.
    Arc::new(Mutex::new(()))
}

pub struct Directory;

impl Directory {
    pub fn lock(&self, name: &str) -> std::io::Result<File> {
        valid_name(name)?;
        Err(unsupported())
    }
    pub fn private(_path: &Path, _create: bool) -> std::io::Result<Self> {
        Err(unsupported())
    }
    pub fn open(_path: &Path, _create: bool) -> std::io::Result<Self> {
        Err(unsupported())
    }
    pub fn read(&self, name: &str, _max: usize, _private: bool) -> std::io::Result<String> {
        valid_name(name)?;
        Err(unsupported())
    }
    pub fn read_private(&self, name: &str, _max: usize) -> std::io::Result<String> {
        valid_name(name)?;
        Err(unsupported())
    }
    pub(crate) fn open_read(
        &self,
        name: &str,
        _max: usize,
        _private: bool,
        _exact: bool,
    ) -> std::io::Result<File> {
        valid_name(name)?;
        Err(unsupported())
    }
    pub(crate) fn append(&self, name: &str, _bytes: &[u8]) -> std::io::Result<()> {
        valid_name(name)?;
        Err(unsupported())
    }
    pub fn names(&self) -> std::io::Result<Vec<String>> {
        Err(unsupported())
    }
    pub fn write(&self, name: &str, _bytes: &[u8]) -> std::io::Result<()> {
        valid_name(name)?;
        Err(unsupported())
    }
    pub fn write_new(&self, name: &str, _bytes: &[u8]) -> std::io::Result<()> {
        valid_name(name)?;
        Err(unsupported())
    }
    pub fn remove(&self, name: &str) -> std::io::Result<()> {
        valid_name(name)?;
        Err(unsupported())
    }
}

fn valid_name(name: &str) -> std::io::Result<()> {
    let stem = name.split('.').next().unwrap_or_default().to_uppercase();
    let numbered_device = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', ':', '\0'])
        || name.ends_with([' ', '.'])
        || matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        || numbered_device
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
            "", ".", "..", "a/b", "a\\b", "a:b", "NUL", "con.txt", "COM1", "lpt9.log", "COM¹",
            "a.", "a ",
        ] {
            assert!(valid_name(name).is_err(), "{name}");
        }
        for name in ["flows.json", "é space.txt", "COM10"] {
            assert!(valid_name(name).is_ok(), "{name}");
        }
    }
    #[test]
    fn storage_refuses_without_creating_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing");
        assert!(Directory::private(&path, true).is_err());
        assert!(!path.exists());
        assert!(Directory.write("flows.json", b"secret").is_err());
    }
}
