use super::*;
use std::os::unix::fs::MetadataExt;
static TRUSTED_SHELL: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Snapshot the trusted launcher at process startup; never resolve it through PATH.
/// Candidates: the usual paths, then bash entries of the root-managed /etc/shells.
pub fn init_shell() {
    TRUSTED_SHELL.get_or_init(|| {
        let listed = std::fs::read_to_string("/etc/shells").unwrap_or_default();
        let found = ["/usr/bin/bash", "/bin/bash"]
            .into_iter()
            .chain(
                listed
                    .lines()
                    .map(str::trim)
                    .filter(|line| line.starts_with('/') && line.ends_with("/bash")),
            )
            .find_map(|path| trusted_launcher(std::path::Path::new(path)));
        found
    });
}

/// Canonical launcher target, with every ancestor locked against non-root replacement.
/// A sticky root-owned directory (NixOS /nix/store) counts only for a root-owned entry.
pub(super) fn trusted_launcher(path: &std::path::Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    let file = canonical.metadata().ok()?;
    if !file.is_file() || file.uid() != 0 || file.mode() & 0o022 != 0 {
        return None;
    }
    let mut child_uid = file.uid();
    for dir in canonical.ancestors().skip(1) {
        let meta = dir.metadata().ok()?;
        let locked = meta.mode() & 0o022 == 0 || (meta.mode() & 0o1000 != 0 && child_uid == 0);
        if !meta.is_dir() || meta.uid() != 0 || !locked {
            return None;
        }
        child_uid = meta.uid();
    }
    Some(canonical)
}

pub(super) fn bash_command() -> Result<Command, ToolError> {
    // Also initialize for library callers that do not use our binary entry points.
    init_shell();
    let shell = TRUSTED_SHELL.get().and_then(Option::as_ref).ok_or_else(|| {
        ToolError::Denied {
            code: "untrusted_shell".into(),
            message: "bash tool unavailable: no trusted bash (root-owned file and directories, not group/other writable unless sticky) at /usr/bin/bash, /bin/bash or in /etc/shells".into(),
        }
    })?;
    let mut command = Command::new(shell);
    command.args(["--noprofile", "--norc", "-c"]);
    for key in [
        "BASH_ENV",
        "ENV",
        "SHELLOPTS",
        "BASHOPTS",
        "PROMPT_COMMAND",
        "CDPATH",
        "GLOBIGNORE",
        "IFS",
    ] {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"BASH_FUNC_") {
            command.env_remove(key);
        }
    }
    Ok(command)
}

/// The same trusted launcher used by the tool, without executing it.
pub fn trusted_shell() -> Option<&'static std::path::Path> {
    init_shell();
    TRUSTED_SHELL.get().and_then(Option::as_deref)
}
