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
    let normalized = |p: &Path| {
        let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let text = canonical
            .to_string_lossy()
            .replace('/', "\\")
            .to_lowercase();
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    };
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
    // No embedded quotes/backslashes: MSYS and MSYS=noglob decode this identically.
    // BASH_EXECUTION_STRING shows this wrapper, not the transported user command.
    command.arg("set -f;IFS=;eval $WITHNAV_BASH_COMMAND");
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

pub(super) fn set_command(launcher: &mut Command, command: &str) {
    // Expansion is one word (empty IFS, no globbing); eval restores shell defaults
    // and removes the transport variable before executing the Captain's command.
    launcher.env(
        "WITHNAV_BASH_COMMAND",
        format!("unset WITHNAV_BASH_COMMAND IFS;set +f;\n{command}"),
    );
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

    #[test]
    fn canonical_wsl_refusal_handles_verbatim_and_dot_components() {
        let root = tempfile::tempdir().unwrap();
        let system32 = root.path().join("System32");
        std::fs::create_dir(&system32).unwrap();
        let bash = system32.join("bash.exe");
        std::fs::write(&bash, "").unwrap();
        let verbatim = bash.canonicalize().unwrap();
        assert!(wsl_launcher(&verbatim, Some(root.path())));
        assert!(wsl_launcher(
            &root.path().join("System32/../System32/bash.exe"),
            Some(root.path())
        ));
        assert!(wsl_launcher(
            &bash,
            Some(&root.path().canonicalize().unwrap())
        ));
    }

    #[tokio::test]
    async fn git_bash_round_trips_command_text_and_exit_status() {
        if trusted_shell().is_none() {
            eprintln!(
                "skipping command round-trip: Git Bash not found in registry, ProgramFiles or PATH"
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let cx = ToolContext {
            binding: None,
            workspace: root.path().to_path_buf(),
            writable_roots: vec![root.path().to_path_buf()],
            readable_roots: vec![],
        };
        for (command, output, exit) in [
            (r#"printf '%s' 'a\\b'"#, r"a\\b", 0),
            (r#"printf '%s' 'a"b'"'"'c'"#, "a\"b'c", 0),
            (r#"printf '%s' 'tail\'"#, r"tail\", 0),
            (
                "printf '%s' first\nprintf '%s' second\nexit 3",
                "firstsecond",
                3,
            ),
            ("true", "", 0),
            (
                "false; result=$?; printf '%s' \"$result\"; exit \"$result\"",
                "1",
                1,
            ),
            ("printf '%s' 'é日本語🧭'", "é日本語🧭", 0),
            ("printf '%s' \"${WITHNAV_BASH_COMMAND-unset}\"", "unset", 0),
        ] {
            let result = run_bash(command, &cx, CancellationToken::new(), 5000, 200)
                .await
                .unwrap();
            assert_eq!(result.output, output, "{command}");
            assert_eq!(result.metadata["exit_code"], exit, "{command}");
            for mode in [None, Some("noglob")] {
                let mut launcher = bash_command().unwrap();
                set_command(&mut launcher, command);
                match mode {
                    None => {
                        launcher.env_remove("MSYS");
                    }
                    Some(value) => {
                        launcher.env("MSYS", value);
                    }
                }
                launcher
                    .current_dir(root.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let (child, _job) =
                    crate::tools::process_job::Job::spawn(&mut launcher, None).unwrap();
                let result = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    String::from_utf8(result.stdout).unwrap(),
                    output,
                    "MSYS={mode:?}: {command}"
                );
                assert_eq!(result.status.code(), Some(exit), "MSYS={mode:?}: {command}");
            }
        }
    }
}
