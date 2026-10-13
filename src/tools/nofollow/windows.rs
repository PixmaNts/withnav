//! Ordinary Windows file access; classification belongs to canonical path validation.
use std::io::{Read, Write};
use std::path::Path;

use crate::error::ToolError;

/// The ordinary path Windows will open, including aliases of existing ancestors.
pub(crate) fn path_identity(path: &Path) -> Result<std::path::PathBuf, ToolError> {
    validate_target(path)?;
    let spelling = path.to_string_lossy().replace('/', "\\");
    let spelling = spelling.strip_prefix("\\\\?\\").unwrap_or(&spelling);
    let mut ancestor = std::path::PathBuf::from(spelling);
    let mut suffix = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(canonical) => {
                validate_target(&canonical)?;
                let text = canonical.to_string_lossy();
                let mut result =
                    std::path::PathBuf::from(text.strip_prefix("\\\\?\\").unwrap_or(&text));
                for part in suffix.into_iter().rev() {
                    result.push(part);
                }
                return Ok(result);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor.file_name().ok_or_else(|| ToolError::Io(error))?;
                // Ordinary Win32 creation trims these characters too. Keep the
                // returned suffix ordinary rather than changing it with a verbatim prefix.
                let name = name
                    .to_string_lossy()
                    .trim_end_matches(['.', ' '])
                    .to_owned();
                if name.is_empty() {
                    return Err(ToolError::PathTraversal(path.to_path_buf()));
                }
                suffix.push(name);
                ancestor.pop();
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub(crate) fn path_within(path: &Path, root: &Path) -> bool {
    let key = |path: &Path| path.to_string_lossy().replace('\\', "/").to_lowercase();
    let path = key(path);
    let root = key(root);
    path == root || path.starts_with(&format!("{}/", root.trim_end_matches('/')))
}

pub(crate) fn drive_root(path: &Path) -> Result<std::path::PathBuf, ToolError> {
    validate_target(path)?;
    let mut parts = path.components();
    let Some(std::path::Component::Prefix(prefix)) = parts.next() else {
        return Err(ToolError::PathTraversal(path.to_path_buf()));
    };
    match prefix.kind() {
        std::path::Prefix::Disk(drive) | std::path::Prefix::VerbatimDisk(drive) => Ok(
            std::path::PathBuf::from(format!("{}:\\", char::from(drive))),
        ),
        _ => Err(ToolError::PathTraversal(path.to_path_buf())),
    }
}

#[cfg(test)]
pub(crate) fn short_alias(path: &Path) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let input: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut output = vec![0u16; 32768];
    let count = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(
            input.as_ptr(),
            output.as_mut_ptr(),
            output.len() as u32,
        )
    } as usize;
    if count == 0 || count >= output.len() {
        eprintln!("skipping short alias: volume does not provide an 8.3 path");
        return None;
    }
    let alias = std::path::PathBuf::from(std::ffi::OsString::from_wide(&output[..count]));
    if alias == path {
        eprintln!("skipping short alias: 8.3 name generation is disabled or no alias exists");
        None
    } else {
        Some(alias)
    }
}

#[cfg(test)]
thread_local! {
    static BEFORE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

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

/// UNC shares are refused consistently, including canonical verbatim UNC paths.
pub(crate) fn validate_target(path: &Path) -> Result<(), ToolError> {
    let text = path.to_string_lossy().replace('/', "\\");
    let upper = text.to_uppercase();
    let plain = upper.strip_prefix("\\\\?\\").unwrap_or(&upper);
    let invalid = upper.starts_with("\\\\.\\")
        || (upper.starts_with("\\\\?\\")
            && !(plain.as_bytes().get(1) == Some(&b':')
                && plain
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphabetic)))
        || plain.starts_with("GLOBALROOT")
        || plain.starts_with("UNC\\")
        || (upper.starts_with("\\\\") && !upper.starts_with("\\\\?\\"))
        || plain.split('\\').enumerate().any(|(index, part)| {
            let drive = index == 0
                && part.len() == 2
                && part.as_bytes()[0].is_ascii_alphabetic()
                && part.ends_with(':');
            let stem = part
                .trim_end_matches([' ', '.'])
                .split('.')
                .next()
                .unwrap_or("");
            (!drive && part.contains(':'))
                || matches!(stem, "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
                || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.len() == 4
                    && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        });
    if invalid {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
            "native file targets must be local ordinary paths (no UNC, device namespace, reserved device name or alternate data stream)").into());
    }
    Ok(())
}

pub fn open_read(destination: &Path) -> Result<std::fs::File, ToolError> {
    before_open();
    validate_target(destination)?;
    let file = std::fs::File::open(destination)?;
    if !file.metadata()?.is_file() {
        return Err(ToolError::DestinationChanged(destination.to_path_buf()));
    }
    Ok(file)
}

pub fn write_atomic(destination: &Path, content: &[u8]) -> Result<(), ToolError> {
    before_open();
    validate_target(destination)?;
    let parent = destination
        .parent()
        .ok_or_else(|| ToolError::DestinationChanged(destination.to_path_buf()))?;
    std::fs::create_dir_all(parent)?;
    match std::fs::metadata(destination) {
        Ok(meta) if !meta.is_file() => {
            return Err(ToolError::DestinationChanged(destination.to_path_buf()))
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(content)?;
    temp.as_file().sync_all()?;
    temp.persist(destination)
        .map_err(|error| ToolError::Io(error.error))?;
    Ok(())
}

pub fn edit_atomic(
    destination: &Path,
    change: impl FnOnce(String) -> Result<(String, String), ToolError>,
) -> Result<String, ToolError> {
    let file = open_read(destination).map_err(|error| match error {
        ToolError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => {
            ToolError::NotFound(destination.to_path_buf())
        }
        other => other,
    })?;
    let mut text = String::new();
    file.take(16 * 1024 * 1024 + 1).read_to_string(&mut text)?;
    if text.len() > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "edit target exceeds 16 MiB",
        )
        .into());
    }
    let (updated, report) = change(text)?;
    write_atomic(destination, updated.as_bytes())?;
    Ok(report)
}

pub fn write_scratch(
    destination: &Path,
    content: &[u8],
    identity: &crate::agent::scratch::Identity,
) -> Result<(), ToolError> {
    if !destination.starts_with(&identity.path) {
        return Err(ToolError::DestinationChanged(destination.to_path_buf()));
    }
    write_atomic(destination, content)
}

pub fn edit_scratch(
    destination: &Path,
    identity: &crate::agent::scratch::Identity,
    change: impl FnOnce(String) -> Result<(String, String), ToolError>,
) -> Result<String, ToolError> {
    if !destination.starts_with(&identity.path) {
        return Err(ToolError::DestinationChanged(destination.to_path_buf()));
    }
    edit_atomic(destination, change)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_follow_the_targets_drive() {
        assert_eq!(
            drive_root(Path::new(r"D:\project\file.txt")).unwrap(),
            Path::new(r"D:\")
        );
        assert_eq!(
            drive_root(Path::new(r"\\?\E:\project\file.txt")).unwrap(),
            Path::new(r"E:\")
        );
    }

    #[test]
    fn authorized_target_on_another_available_drive() {
        let workspace = tempfile::tempdir().unwrap();
        let current = drive_root(workspace.path()).unwrap();
        let cx = crate::tools::ToolContext {
            binding: None,
            workspace: workspace.path().to_path_buf(),
            writable_roots: vec![],
            readable_roots: vec![],
        };
        let Some(root) = (b'A'..=b'Z')
            .map(|drive| std::path::PathBuf::from(format!("{}:\\", char::from(drive))))
            .find(|root| *root != current && root.is_dir())
        else {
            eprintln!("skipping second-drive test: only one available drive");
            return;
        };
        let resolved = crate::agent::permissions::resolve_read_target(
            &root.join("withnav-missing-test-file"),
            &cx,
        )
        .unwrap();
        assert!(path_within(&resolved, &root));
    }

    #[test]
    fn ordinary_atomic_writes_and_exact_edit_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new/deep/file.txt");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert!(matches!(
            edit_atomic(&path, |_| Err(ToolError::NoMatch)),
            Err(ToolError::NoMatch)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(
            edit_atomic(&path, |text| Ok((text + "!", "edited".into()))).unwrap(),
            "edited"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two!");
        assert!(matches!(
            edit_atomic(&dir.path().join("missing/file"), |_| unreachable!()),
            Err(ToolError::NotFound(_))
        ));
        assert!(!dir.path().join("missing").exists());
        assert!(write_atomic(dir.path(), b"bad").is_err());
    }

    #[test]
    fn rejects_nonordinary_file_targets() {
        for path in [
            r"\\.\NUL",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\x",
            r"\\server\share\x",
            r"\\?\UNC\server\share\x",
            r"C:\x:stream",
            r"C:\NUL.txt",
            r"C:\com1",
            r"C:\LPT9.log",
        ] {
            assert!(validate_target(Path::new(path)).is_err(), "{path}");
        }
        for path in [r"C:\ordinary\file.txt", r"\\?\C:\ordinary\file.txt"] {
            validate_target(Path::new(path)).unwrap();
        }
    }
}
