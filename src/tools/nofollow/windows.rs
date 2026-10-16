//! Ordinary Windows file access; classification belongs to canonical path validation.
use std::io::{Read, Write};
use std::path::Path;

use crate::error::ToolError;

/// Native paths have no shell expansion; root-relative paths retain the workspace drive.
pub(crate) fn requested_path(
    path: &Path,
    workspace: &Path,
    home: Option<&Path>,
) -> Result<std::path::PathBuf, ToolError> {
    if path.as_os_str().is_empty() || path.to_string_lossy().contains('\0') {
        return Err(ToolError::PathTraversal(path.to_path_buf()));
    }
    if path.starts_with("~") {
        Ok(home
            .ok_or_else(|| ToolError::PathTraversal(path.to_path_buf()))?
            .join(
                path.strip_prefix("~")
                    .map_err(|_| ToolError::PathTraversal(path.to_path_buf()))?,
            ))
    } else if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(workspace.join(path))
    }
}

pub(crate) fn resolve_target(
    path: &Path,
    workspace: &Path,
    home: Option<&Path>,
) -> Result<std::path::PathBuf, ToolError> {
    path_identity(&requested_path(path, workspace, home)?)
}

/// The ordinary path Windows will open, including aliases of existing ancestors.
pub(crate) fn path_identity(path: &Path) -> Result<std::path::PathBuf, ToolError> {
    validate_target(path)?;
    let spelling = path.to_string_lossy().replace('/', "\\");
    let mut ancestor = std::path::PathBuf::from(&spelling);
    let mut suffix = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(canonical) => {
                validate_target(&canonical)?;
                let mut result = canonical;
                for part in suffix.into_iter().rev() {
                    result.push(part);
                }
                let text = result.to_string_lossy();
                let plain = text.strip_prefix("\\\\?\\").unwrap_or(&text);
                // Verbatim paths can name entries ordinary Win32 syntax would
                // trim or reinterpret. Simplify only an equivalent spelling.
                let ordinary = plain.encode_utf16().count() < 260
                    && plain.split('\\').all(|part| !part.ends_with(['.', ' ']));
                return Ok(std::path::PathBuf::from(if ordinary {
                    plain
                } else {
                    &text
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor.file_name().ok_or(ToolError::Io(error))?;
                let name = name.to_string_lossy().into_owned();
                if name != "." && name != ".." && name.ends_with(['.', ' ']) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "creating Windows path components ending in a dot or space is not supported",
                    ).into());
                }
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
    let path = crate::tools::resolved_path_key(path);
    let root = crate::tools::resolved_path_key(root);
    path.starts_with(root)
}

#[cfg(test)]
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
    validate_target(destination)?;
    let file = std::fs::File::open(destination)?;
    if !file.metadata()?.is_file() {
        return Err(ToolError::DestinationChanged(destination.to_path_buf()));
    }
    Ok(file)
}

pub fn write_atomic(destination: &Path, content: &[u8]) -> Result<(), ToolError> {
    validate_target(destination)?;
    let parent = destination
        .parent()
        .ok_or_else(|| ToolError::DestinationChanged(destination.to_path_buf()))?;
    std::fs::create_dir_all(parent)?;
    match std::fs::metadata(destination) {
        Ok(meta) if !meta.is_file() => {
            return Err(ToolError::DestinationChanged(destination.to_path_buf()));
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
    if !path_within(destination, &identity.path) {
        return Err(ToolError::DestinationChanged(destination.to_path_buf()));
    }
    write_atomic(destination, content)
}

pub fn edit_scratch(
    destination: &Path,
    identity: &crate::agent::scratch::Identity,
    change: impl FnOnce(String) -> Result<(String, String), ToolError>,
) -> Result<String, ToolError> {
    if !path_within(destination, &identity.path) {
        return Err(ToolError::DestinationChanged(destination.to_path_buf()));
    }
    edit_atomic(destination, change)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_root_contains_resolved_targets_and_allows_scratch_writes() {
        let dir = tempfile::Builder::new()
            .prefix("withnav long directory name ")
            .tempdir()
            .unwrap();
        let Some(short) = short_alias(dir.path()) else {
            return;
        };
        let target = resolve_target(&short.join("note.txt"), &short, None).unwrap();
        eprintln!(
            "short root: {}; resolved target: {}",
            short.display(),
            target.display()
        );
        assert!(path_within(&target, &short));
        let cx = crate::tools::ToolContext {
            workspace: short.clone(),
            writable_roots: vec![short.clone()],
            readable_roots: vec![],
            binding: None,
        };
        assert_eq!(
            crate::tools::validate_path(&cx.writable_roots, &target).unwrap(),
            target
        );
        let identity = crate::agent::scratch::Identity {
            path: short.clone(),
        };
        write_scratch(&target, b"scratch", &identity).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"scratch");
        let floor = crate::permission::floor::FloorContext {
            workspace: short.clone(),
            config_dir: Some(short.clone()),
            data_dir: Some(short.clone()),
            ..Default::default()
        };
        for name in ["config.json", "withnav.db"] {
            let protected = resolve_target(&short.join(name), &short, None).unwrap();
            assert!(crate::permission::floor::protected_write(
                &protected, &floor
            ));
        }
    }

    #[test]
    fn long_target_identity_is_stable_and_deny_rules_ignore_display_prefix() {
        use crate::permission::rules::{Effect, RuleOrigin, RuleScope, literal};
        let dir = tempfile::tempdir().unwrap();
        let target = dir
            .path()
            .join("a".repeat(110))
            .join("b".repeat(110))
            .join("file.txt");
        let before = resolve_target(&target, dir.path(), None).unwrap();
        assert!(before.to_string_lossy().starts_with(r"\\?\"));
        assert!(path_within(
            &before,
            &crate::tools::canonical_path(dir.path()).unwrap()
        ));
        let store = crate::permission::store::PermissionStore::open(dir.path()).unwrap();
        let rule = store
            .add(
                "write",
                &literal(&before.to_string_lossy()),
                Effect::Deny,
                RuleScope::Global,
                RuleOrigin::Manual,
            )
            .unwrap();
        write_atomic(&before, b"long").unwrap();
        let after = resolve_target(&target, dir.path(), None).unwrap();
        assert_eq!(before, after);
        for action in ["*", "writ*"] {
            let deny = store
                .add(
                    action,
                    &literal(&before.to_string_lossy()),
                    Effect::Deny,
                    RuleScope::Global,
                    RuleOrigin::Manual,
                )
                .unwrap();
            store.revoke(rule.id).unwrap();
            assert_eq!(
                store
                    .matching("write", &after.to_string_lossy(), dir.path(), "build")
                    .unwrap()
                    .unwrap()
                    .id,
                deny.id
            );
            store.revoke(deny.id).unwrap();
        }
        let rule = store
            .add(
                "write",
                &literal(&before.to_string_lossy()),
                Effect::Deny,
                RuleScope::Global,
                RuleOrigin::Manual,
            )
            .unwrap();
        for spelling in [
            after.to_string_lossy().into_owned(),
            after
                .to_string_lossy()
                .strip_prefix(r"\\?\")
                .unwrap()
                .to_owned(),
        ] {
            assert_eq!(
                store
                    .matching("write", &spelling, dir.path(), "build")
                    .unwrap()
                    .unwrap()
                    .id,
                rule.id
            );
        }
        assert_eq!(
            crate::tools::validate_path(&[dir.path().to_path_buf()], &target).unwrap(),
            after
        );
    }

    #[test]
    fn missing_verbatim_dot_and_space_names_are_refused_without_touching_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let ordinary = dir.path().join("note");
        std::fs::write(&ordinary, "unchanged").unwrap();
        for name in ["note.", "note "] {
            let target = dir.path().canonicalize().unwrap().join(name);
            assert!(matches!(resolve_target(&target, dir.path(), None),
                Err(ToolError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput));
            assert_eq!(std::fs::read_to_string(&ordinary).unwrap(), "unchanged");
            assert!(!target.exists());
            std::fs::write(&target, "existing").unwrap();
            let resolved = resolve_target(&target, dir.path(), None).unwrap();
            let mut text = String::new();
            open_read(&resolved)
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            assert_eq!(text, "existing");
            assert_eq!(std::fs::read_to_string(&ordinary).unwrap(), "unchanged");
        }
    }

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
