//! The fixed floor: credentials and WithNav's own configuration and database.
//!
//! Only yolo skips it. Credentials ask for reads and writes alike; the
//! configuration, the database and the global flows directory are free to read
//! but need the permission flow's protected check before they are written.
//! For bash the floor is best effort: it looks at literal path words,
//! redirection targets and WithNav's own rule-changing commands. It does not
//! expand globs or variables, and it is not a sandbox.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::shell::ShellCommand;
use crate::provider::ToolCall;

/// Credential names are host facts, independent of a model's classification.
pub const SENSITIVE_NAMES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".netrc",
    "auth.json",
    "keyrings",
    ".aws",
    "gcloud",
    "credentials",
    "credentials.json",
    "application_default_credentials.json",
    ".env*",
    "*id_rsa*",
    "*id_ed25519*",
    "*id_ecdsa*",
    "*.pem",
    "*.key",
    "*.keyring",
];

pub fn sensitive_read(path: &std::path::Path) -> bool {
    static MATCHER: std::sync::OnceLock<globset::GlobSet> = std::sync::OnceLock::new();
    let matcher = MATCHER.get_or_init(|| {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in SENSITIVE_NAMES {
            builder.add(
                globset::GlobBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .expect("fixed sensitive pattern"),
            );
        }
        builder.build().expect("fixed sensitive matcher")
    });
    path.components()
        .any(|part| matcher.is_match(std::path::Path::new(part.as_os_str())))
}

/// Where the floor and the decision path look: the project, the directories
/// WithNav owns and the home directory that `~` names.
#[derive(Debug, Clone, Default)]
pub struct FloorContext {
    pub workspace: PathBuf,
    /// Where the agent may change files; part of the project.
    pub writable_roots: Vec<PathBuf>,
    /// Extra places `read`, `grep` and `glob` may look, never change.
    pub readable_roots: Vec<PathBuf>,
    /// Holds `config.json` and the global flows directory.
    pub config_dir: Option<PathBuf>,
    /// Holds `withnav.db` and its write-ahead files.
    pub data_dir: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

impl FloorContext {
    /// The roots of a tool context with the configured config and data
    /// directories of this machine.
    pub fn from_environment(cx: &crate::tools::ToolContext) -> Self {
        Self {
            workspace: cx.workspace.clone(),
            writable_roots: cx.writable_roots.clone(),
            readable_roots: cx.readable_roots.clone(),
            config_dir: crate::provider::auth::default_config_dir(),
            data_dir: crate::paths::data_dir(),
            home: directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()),
        }
    }
}

/// What the floor says about a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Floor {
    None,
    /// A credential is touched; the string names the target.
    Credential(String),
    /// WithNav's own configuration or database is written; the string names the target.
    Protected(String),
}

/// Whether the path names a credential: keys, `auth.json`, `.env` files and the
/// other names of [`SENSITIVE_NAMES`], in any path component.
pub fn credential(path: &Path) -> bool {
    sensitive_read(path)
}

/// Whether writing the path changes `config.json`, the database or the global
/// flows directory. The path is resolved first; reading these files is free.
pub fn protected_write(path: &Path, cx: &FloorContext) -> bool {
    let path = resolve_path(&path.to_string_lossy(), cx);
    path.is_some_and(|path| protected_resolved(&path, cx))
}

fn protected_resolved(path: &Path, cx: &FloorContext) -> bool {
    let canonical = |dir: &Path| resolve_path(&dir.to_string_lossy(), cx);
    if let Some(data) = cx.data_dir.as_deref().and_then(canonical) {
        for name in ["withnav.db", "withnav.db-wal", "withnav.db-shm"] {
            if path == data.join(name) {
                return true;
            }
        }
    }
    if let Some(config) = cx.config_dir.as_deref().and_then(canonical) {
        if path == config.join("config.json") || path.starts_with(config.join("flows")) {
            return true;
        }
    }
    false
}

/// Resolve a path the way a tool would: `~` is the home directory, a relative
/// path is anchored in the workspace, `.` and `..` are folded, and the nearest
/// existing ancestor is canonicalised so symlinks cannot hide a target.
pub fn resolve_path(path: &str, cx: &FloorContext) -> Option<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    let joined = if path == "~" {
        cx.home.clone()?
    } else if let Some(rest) = path.strip_prefix("~/") {
        cx.home.as_ref()?.join(rest)
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cx.workspace.join(path)
    };
    let mut folded = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                folded.pop();
            }
            Component::CurDir => {}
            other => folded.push(other),
        }
    }
    let mut existing = folded.clone();
    let mut tail = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_owned());
                existing.pop();
            }
            None => break,
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    resolved.extend(tail.into_iter().rev());
    Some(resolved)
}

/// The floor for one call. `commands` are the split commands of a `bash` call
/// (empty for other tools). Native tools are judged by their `path` argument;
/// credentials come before protected targets.
pub fn check(call: &ToolCall, commands: &[ShellCommand], cx: &FloorContext) -> Floor {
    match call.name.as_str() {
        "read" | "grep" | "glob" => native(call, cx, false),
        "write" | "edit" => native(call, cx, true),
        "bash" => commands
            .iter()
            .map(|command| bash(command, cx))
            .min_by_key(|floor| match floor {
                Floor::Credential(_) => 0,
                Floor::Protected(_) => 1,
                Floor::None => 2,
            })
            .unwrap_or(Floor::None),
        _ => Floor::None,
    }
}

fn native(call: &ToolCall, cx: &FloorContext, writes: bool) -> Floor {
    let Some(path) = call.args.get("path").and_then(Value::as_str) else {
        return Floor::None;
    };
    if names_credential(path, cx) {
        return Floor::Credential(path.to_owned());
    }
    if writes && names_protected(path, cx) {
        return Floor::Protected(path.to_owned());
    }
    Floor::None
}

/// The spelling and the resolved target are both checked: a link named
/// innocently can point at a key.
fn names_credential(word: &str, cx: &FloorContext) -> bool {
    let plain: String = word.chars().filter(|c| !"[]{}*?\\".contains(*c)).collect();
    credential(Path::new(&plain)) || resolve_path(word, cx).is_some_and(|path| credential(&path))
}

fn names_protected(word: &str, cx: &FloorContext) -> bool {
    resolve_path(word, cx).is_some_and(|path| protected_resolved(&path, cx))
}

/// Programs that only read their operands, so naming a protected file is free.
const READERS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "grep",
    "rg",
    "ls",
    "wc",
    "stat",
    "file",
    "diff",
    "jq",
    "bat",
    "sha256sum",
    "md5sum",
    "cut",
    "sort",
    "echo",
];

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Words that may name a file: everything after the program, with options
/// dropped and `--option=value` reduced to its value. An opaque command has no
/// parsed words, so its raw text is scanned token by token instead.
fn source_words(command: &ShellCommand) -> Vec<String> {
    if command.opaque {
        command
            .raw
            .split(|c: char| c.is_whitespace() || "'\"`;|&()<>".contains(c))
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        command.words.clone()
    }
}

fn operands(command: &ShellCommand, words: Vec<String>) -> (Option<String>, Vec<String>) {
    let start = if command.opaque {
        0
    } else {
        words.iter().take_while(|word| is_assignment(word)).count()
    };
    let program = (!command.opaque)
        .then(|| words.get(start).cloned())
        .flatten();
    let skip = start + usize::from(program.is_some());
    let operands = words
        .into_iter()
        .skip(skip)
        .filter_map(
            |word| match word.strip_prefix("--").and_then(|w| w.split_once('=')) {
                Some((_, value)) => Some(value.to_owned()),
                None if word.starts_with('-') => None,
                None => Some(word),
            },
        )
        .collect();
    (program, operands)
}

fn bash(command: &ShellCommand, cx: &FloorContext) -> Floor {
    let all = source_words(command);
    let (program, operands) = operands(command, all.clone());
    let program = program.as_deref().map(basename);
    let targets = || {
        command
            .redirections
            .iter()
            .map(|redirection| redirection.target.as_str())
    };
    if let Some(word) = operands
        .iter()
        .map(String::as_str)
        .chain(targets())
        .find(|word| names_credential(word, cx))
    {
        return Floor::Credential(word.to_owned());
    }
    // A redirection writes unless it only reads its target.
    let written = command
        .redirections
        .iter()
        .filter(|redirection| {
            !redirection
                .op
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .starts_with('<')
        })
        .map(|redirection| redirection.target.as_str());
    let reads_only = program.is_some_and(|program| READERS.contains(&program));
    let candidates: Vec<&str> = if reads_only {
        written.collect()
    } else {
        operands.iter().map(String::as_str).chain(written).collect()
    };
    if let Some(word) = candidates
        .into_iter()
        .find(|word| names_protected(word, cx))
    {
        return Floor::Protected(word.to_owned());
    }
    let words = || all.iter().map(|word| basename(word));
    if (command.opaque || words().any(|word| matches!(word, "sqlite3" | "sqlite")))
        && operands
            .iter()
            .any(|word| basename(word).starts_with("withnav.db"))
    {
        return Floor::Protected("withnav.db".into());
    }
    let words: Vec<&str> = words().collect();
    if words.contains(&"nav")
        && words
            .windows(2)
            .any(|pair| pair[0] == "permissions" && matches!(pair[1], "add" | "revoke"))
    {
        return Floor::Protected("nav permissions".into());
    }
    Floor::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::shell::split;
    use serde_json::json;

    struct Fixture {
        _dir: tempfile::TempDir,
        cx: FloorContext,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["project", "config/flows", "data", "home/.ssh"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let cx = FloorContext {
            workspace: root.join("project"),
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
            config_dir: Some(root.join("config")),
            data_dir: Some(root.join("data")),
            home: Some(root.join("home")),
        };
        Fixture { _dir: dir, cx }
    }

    fn native_call(name: &str, path: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            args: json!({"path": path}),
            args_valid: true,
        }
    }

    fn bash_floor(cx: &FloorContext, line: &str) -> Floor {
        let call = ToolCall {
            id: "c".into(),
            name: "bash".into(),
            args: json!({"command": line}),
            args_valid: true,
        };
        check(&call, &split(line), cx)
    }

    #[test]
    fn credential_names() {
        for path in [
            "/home/u/.config/withnav/auth.json",
            "/home/u/.ssh/id_ed25519",
            "/home/u/.ssh/id_rsa.pub",
            "/work/.env",
            "/work/.env.production",
            "/work/server.pem",
        ] {
            assert!(credential(Path::new(path)), "{path}");
        }
        for path in [
            "/work/src/main.rs",
            "/work/environment.rs",
            "/work/envelope",
        ] {
            assert!(!credential(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn protected_targets_are_written_never_read() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().to_path_buf();
        for path in [
            "config/config.json",
            "data/withnav.db",
            "data/withnav.db-wal",
            "data/withnav.db-shm",
            "config/flows/mine/flow.json",
            "project/../config/config.json",
        ] {
            let full = root.join(path);
            assert!(protected_write(&full, &f.cx), "{path}");
            let name = full.to_string_lossy().into_owned();
            assert_eq!(
                check(&native_call("write", &name), &[], &f.cx),
                Floor::Protected(name.clone())
            );
            assert_eq!(
                check(&native_call("edit", &name), &[], &f.cx),
                Floor::Protected(name.clone())
            );
            assert_eq!(check(&native_call("read", &name), &[], &f.cx), Floor::None);
        }
        for path in [
            "config/models.json",
            "data/withnav.jsonl",
            "project/config.json",
        ] {
            assert!(!protected_write(&root.join(path), &f.cx), "{path}");
        }
    }

    #[test]
    fn symlinks_cannot_hide_a_protected_file_or_a_key() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().to_path_buf();
        std::fs::write(root.join("config/config.json"), "{}").unwrap();
        std::fs::write(root.join("home/.ssh/id_ed25519"), "key").unwrap();
        std::os::unix::fs::symlink(root.join("config/config.json"), root.join("project/alias"))
            .unwrap();
        std::os::unix::fs::symlink(
            root.join("home/.ssh/id_ed25519"),
            root.join("project/notes"),
        )
        .unwrap();
        assert!(matches!(
            check(&native_call("write", "alias"), &[], &f.cx),
            Floor::Protected(_)
        ));
        assert!(matches!(
            check(&native_call("read", "notes"), &[], &f.cx),
            Floor::Credential(_)
        ));
    }

    #[test]
    fn native_tools_are_judged_by_their_path() {
        let f = fixture();
        assert_eq!(
            check(&native_call("write", "src/a.rs"), &[], &f.cx),
            Floor::None
        );
        assert!(matches!(
            check(&native_call("read", "~/.ssh/id_ed25519"), &[], &f.cx),
            Floor::Credential(_)
        ));
        assert!(matches!(
            check(&native_call("write", ".env"), &[], &f.cx),
            Floor::Credential(_)
        ));
        let other = ToolCall {
            id: "c".into(),
            name: "mcp__s__t".into(),
            args: json!({"path": ".env"}),
            args_valid: true,
        };
        assert_eq!(check(&other, &[], &f.cx), Floor::None);
    }

    #[test]
    fn bash_looks_at_path_words_and_redirections() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().display().to_string();
        assert_eq!(bash_floor(&f.cx, "ls -la && cargo test"), Floor::None);
        assert!(matches!(
            bash_floor(&f.cx, "cat .env"),
            Floor::Credential(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, "echo hi > ~/.ssh/id_ed25519"),
            Floor::Credential(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, "git status && cat --file=.env.local"),
            Floor::Credential(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, &format!("echo x > {root}/config/config.json")),
            Floor::Protected(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, &format!("cp a {root}/data/withnav.db")),
            Floor::Protected(_)
        ));
        // Reading is free.
        assert_eq!(
            bash_floor(&f.cx, &format!("cat {root}/config/config.json")),
            Floor::None
        );
        assert_eq!(
            bash_floor(&f.cx, &format!("sqlite3 {root}/data/other.db .tables")),
            Floor::None
        );
        assert!(matches!(
            bash_floor(
                &f.cx,
                &format!("sqlite3 {root}/data/withnav.db 'drop table x'")
            ),
            Floor::Protected(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, "nav permissions add bash '*' allow"),
            Floor::Protected(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, "sudo nav permissions revoke 3"),
            Floor::Protected(_)
        ));
        assert_eq!(bash_floor(&f.cx, "nav permissions list"), Floor::None);
    }

    #[test]
    fn credentials_win_over_protected_and_opaque_lines_are_scanned() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().display().to_string();
        assert!(matches!(
            bash_floor(&f.cx, &format!("cp {root}/config/config.json x; cat .env")),
            Floor::Credential(_)
        ));
        let opaque = "if true; then cat .env; fi";
        assert!(split(opaque)[0].opaque);
        assert!(matches!(bash_floor(&f.cx, opaque), Floor::Credential(_)));
    }

    #[test]
    fn old_import_path_still_resolves() {
        assert!(crate::agent::permission_gate::sensitive_read(Path::new(
            "/x/auth.json"
        )));
        assert!(!crate::agent::permission_gate::SENSITIVE_NAMES.is_empty());
    }
}
