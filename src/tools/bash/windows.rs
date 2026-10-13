use super::*;
use std::path::Path;
use windows_sys::Win32::System::Registry::*;

static SHELL: OnceLock<Option<PathBuf>> = OnceLock::new();
pub fn init_shell() {
    SHELL.get_or_init(|| {
        discover(
            registry_paths(),
            std::env::var_os("ProgramFiles").map(PathBuf::from),
            std::env::var_os("PATH")
                .map(|v| std::env::split_paths(&v).collect())
                .unwrap_or_default(),
            std::env::var_os("SystemRoot").map(PathBuf::from),
            |p| p.is_file(),
        )
    });
}

fn discover(
    registry: Vec<PathBuf>,
    program_files: Option<PathBuf>,
    path: Vec<PathBuf>,
    system_root: Option<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    registry
        .into_iter()
        .map(|p| p.join("bin/bash.exe"))
        .chain(program_files.map(|p| p.join("Git/bin/bash.exe")))
        .chain(path.into_iter().map(|p| p.join("bash.exe")))
        .find(|p| !wsl_launcher(p, system_root.as_deref()) && exists(p))
}

fn wsl_launcher(path: &Path, system_root: Option<&Path>) -> bool {
    let normalized = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    let path = normalized(path);
    [Some(Path::new(r"C:\Windows")), system_root]
        .into_iter()
        .flatten()
        .any(|root| {
            let root = normalized(root);
            path == root || path.starts_with(&(root.trim_end_matches('\\').to_owned() + "\\"))
        })
}

fn registry_paths() -> Vec<PathBuf> {
    let key: Vec<u16> = "SOFTWARE\\GitForWindows"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let value: Vec<u16> = "InstallPath".encode_utf16().chain(Some(0)).collect();
    [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER]
        .into_iter()
        .filter_map(|root| {
            let mut buffer = [0u16; 32768];
            let mut bytes = std::mem::size_of_val(&buffer) as u32;
            let result = unsafe {
                RegGetValueW(
                    root,
                    key.as_ptr(),
                    value.as_ptr(),
                    RRF_RT_REG_SZ,
                    std::ptr::null_mut(),
                    buffer.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            if result != 0 {
                return None;
            }
            let length = buffer.iter().position(|v| *v == 0)?;
            Some(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
        })
        .collect()
}

pub fn trusted_shell() -> Option<&'static Path> {
    init_shell();
    SHELL.get().and_then(Option::as_deref)
}

pub(super) fn bash_command() -> Result<Command, ToolError> {
    let shell = trusted_shell().ok_or_else(|| ToolError::Denied {
        code: "shell_not_found".into(),
        message: "bash tool unavailable: Git Bash not found in HKLM/HKCU SOFTWARE\\GitForWindows InstallPath, %ProgramFiles%\\Git\\bin\\bash.exe or PATH (Windows/WSL launchers are excluded)".into(),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_prefers_registry_then_program_files_then_path() {
        let registry = PathBuf::from(r"D:\RegistryGit");
        let program = PathBuf::from(r"C:\Program Files");
        let path = PathBuf::from(r"D:\PathGit");
        for expected in [
            registry.join("bin/bash.exe"),
            program.join("Git/bin/bash.exe"),
            path.join("bash.exe"),
        ] {
            let found = discover(
                vec![registry.clone()],
                Some(program.clone()),
                vec![path.clone()],
                None,
                |p| p == expected,
            );
            assert_eq!(found, Some(expected));
        }
        assert_eq!(
            discover(
                vec![registry.clone()],
                Some(program),
                vec![path],
                None,
                |_| true
            ),
            Some(registry.join("bin/bash.exe"))
        );
    }
    #[test]
    fn wsl_launchers_are_excluded_case_insensitively() {
        assert!(wsl_launcher(
            Path::new(r"c:\WINDOWS\system32\bash.exe"),
            None
        ));
        assert!(wsl_launcher(
            Path::new(r"D:\WinNT\System32\bash.exe"),
            Some(Path::new(r"d:\winnt"))
        ));
        assert!(!wsl_launcher(Path::new(r"C:\WindowsTools\bash.exe"), None));
        assert_eq!(
            discover(
                vec![],
                None,
                vec![
                    PathBuf::from(r"C:\Windows\System32"),
                    PathBuf::from(r"C:\Git\bin")
                ],
                None,
                |_| true
            ),
            Some(PathBuf::from(r"C:\Git\bin\bash.exe"))
        );
    }
}
