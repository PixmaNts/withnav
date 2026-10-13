//! The fixed floor: credentials and WithNav's own configuration and database.
//!
//! Only yolo skips it. Credentials ask for reads and writes alike; the
//! configuration, the database and the global flows directory are free to read
//! but need the permission flow's protected check before they are written.
//! For bash the floor is best effort: it looks at literal path words,
//! redirection targets and WithNav's own rule-changing commands. It does not
//! expand globs or variables, and it is not a sandbox.

use std::{
    collections::VecDeque,
    path::{Component, Path, PathBuf},
};

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
    /// A path could not be resolved the way execution would resolve it, so what
    /// it names is unknown. Nothing but yolo allows it.
    Unresolved(String),
}

/// Whether the path names a credential: keys, `auth.json`, `.env` files and the
/// other names of [`SENSITIVE_NAMES`], in any path component.
pub fn credential(path: &Path) -> bool {
    sensitive_read(path)
}

/// Whether writing the path changes `config.json`, the database or the global
/// flows directory. The path is resolved like execution resolves it; a path that
/// cannot be resolved counts as protected. Reading these files is free.
pub fn protected_write(path: &Path, cx: &FloorContext) -> bool {
    resolve_path(&path.to_string_lossy(), cx).is_none_or(|path| protected_resolved(&path, cx))
}

/// Every spelling under which a protected file or the flows root can be
/// reached: the configured path resolved like any candidate (so a link at the
/// final component is followed) and the resolved directory plus the file name
/// (so a link that cannot be followed keeps its place).
fn protected_targets(cx: &FloorContext) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut files = Vec::new();
    let mut roots = Vec::new();
    let mut add = |dir: &Option<PathBuf>, name: &str, into_roots: bool| {
        let Some(dir) = dir else { return };
        let configured = dir.join(name);
        let spellings = [
            resolve_path(&configured.to_string_lossy(), cx),
            resolve_path(&dir.to_string_lossy(), cx).map(|dir| dir.join(name)),
            Some(configured),
        ];
        for spelling in spellings.into_iter().flatten() {
            if into_roots { &mut roots } else { &mut files }.push(spelling);
        }
    };
    add(&cx.config_dir, "config.json", false);
    add(&cx.config_dir, "flows", true);
    for name in ["withnav.db", "withnav.db-wal", "withnav.db-shm"] {
        add(&cx.data_dir, name, false);
    }
    (files, roots)
}

fn protected_resolved(path: &Path, cx: &FloorContext) -> bool {
    let (files, roots) = protected_targets(cx);
    files.iter().any(|file| path == file) || roots.iter().any(|root| path.starts_with(root))
}

/// Links followed before a path counts as a loop, as the kernel does.
const MAX_LINKS: usize = 40;

/// Resolve a path the way execution does: `~` is the home directory, a
/// relative path is anchored in the workspace, and links are followed one
/// component at a time, so a `..` after a symlink climbs out of the link's
/// target, not out of its spelling. A dangling link is followed to where a
/// write would create the file. `None` when resolution fails (a link loop, an
/// unreadable directory, a NUL byte); callers fail closed.
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
    let mut queue: VecDeque<std::ffi::OsString> = VecDeque::new();
    let mut resolved = PathBuf::new();
    let push_all = |queue: &mut VecDeque<_>, path: &Path| {
        for part in path.components().rev() {
            queue.push_front(part.as_os_str().to_owned());
        }
    };
    push_all(&mut queue, &joined);
    let (mut links, mut missing) = (0, false);
    while let Some(part) = queue.pop_front() {
        match Path::new(&part).components().next()? {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                if missing {
                    continue;
                }
                match std::fs::symlink_metadata(&resolved) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        links += 1;
                        if links > MAX_LINKS {
                            return None;
                        }
                        let target = std::fs::read_link(&resolved).ok()?;
                        resolved.pop();
                        push_all(&mut queue, &target);
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
                    Err(_) => return None,
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                resolved = PathBuf::from(&part);
            }
        }
    }
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
                Floor::Unresolved(_) => 1,
                Floor::Protected(_) => 2,
                Floor::None => 3,
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
    match resolve_path(path, cx) {
        None => Floor::Unresolved(path.to_owned()),
        Some(resolved) if writes && protected_resolved(&resolved, cx) => {
            Floor::Protected(path.to_owned())
        }
        Some(_) => Floor::None,
    }
}

/// The spelling and the resolved target are both checked: a link named
/// innocently can point at a key.
fn names_credential(word: &str, cx: &FloorContext) -> bool {
    let plain: String = word.chars().filter(|c| !"[]{}*?\\".contains(*c)).collect();
    credential(Path::new(&plain)) || resolve_path(word, cx).is_some_and(|path| credential(&path))
}

/// Programs that only read their operands, so naming a protected file is free.
/// A program with an output option is a reader only without it, see
/// [`writes_output`].
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

/// Whether a reader is told to write a file: `sort -o`/`--output`, and the
/// `less` log file options. Short options may be clustered or carry the file
/// name (`-uo out`, `-oout`).
fn writes_output(program: &str, words: &[String]) -> bool {
    let (short, long): (&[char], &[&str]) = match program {
        "sort" => (&['o'], &["--output"]),
        "less" => (&['o', 'O'], &["--log-file", "--LOG-FILE"]),
        _ => return false,
    };
    words
        .iter()
        .take_while(|word| *word != "--")
        .any(|word| match word.strip_prefix("--") {
            Some(_) => long.iter().any(|long| {
                word == long
                    || word
                        .strip_prefix(long)
                        .is_some_and(|rest| rest.starts_with('='))
            }),
            None => word.starts_with('-') && word.chars().skip(1).any(|c| short.contains(&c)),
        })
}

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

/// The words of an opaque command, split like a shell would without
/// understanding it: separators and redirection characters end a word outside
/// quotes, quoted text and escaped characters stay inside it with their
/// quotes removed, and nothing is expanded. A quoted path with spaces is one
/// word. The command stays opaque for rules and "Always".
fn scan_words(raw: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => match chars.next() {
                Some(next @ ('"' | '\\' | '$' | '`')) => word.push(next),
                Some(next) => {
                    word.push('\\');
                    word.push(next);
                }
                None => word.push('\\'),
            },
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, '\\') => {
                started = true;
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            (None, c) if c.is_whitespace() || ";|&()<>`".contains(c) => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

fn source_words(command: &ShellCommand) -> Vec<String> {
    if command.opaque {
        scan_words(&command.raw)
    } else {
        command.words.clone()
    }
}

/// Words that may name a file: everything after the program, with options
/// dropped and `--option=value` reduced to its value.
fn operands(
    command: &ShellCommand,
    words: &[String],
) -> (Option<String>, Vec<String>, Vec<String>) {
    let start = if command.opaque {
        0
    } else {
        words.iter().take_while(|word| is_assignment(word)).count()
    };
    let program = (!command.opaque)
        .then(|| words.get(start).cloned())
        .flatten();
    let skip = start + usize::from(program.is_some());
    let rest = words[skip.min(words.len())..].to_vec();
    let operands = rest
        .iter()
        .filter_map(
            |word| match word.strip_prefix("--").and_then(|w| w.split_once('=')) {
                Some((_, value)) => Some(value.to_owned()),
                None if word.starts_with('-') => None,
                None => Some(word.clone()),
            },
        )
        .collect();
    (program, rest, operands)
}

/// Redirections that only read their target. `<>` opens it for writing too.
fn reads_only_redirection(op: &str) -> bool {
    matches!(
        op.trim_start_matches(|c: char| c.is_ascii_digit()),
        "<" | "<<" | "<<<" | "<<-" | "<&"
    )
}

fn bash(command: &ShellCommand, cx: &FloorContext) -> Floor {
    let all = source_words(command);
    let (program, rest, operands) = operands(command, &all);
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
    let written = command
        .redirections
        .iter()
        .filter(|redirection| !reads_only_redirection(&redirection.op))
        .map(|redirection| redirection.target.as_str());
    let reads_only =
        program.is_some_and(|program| READERS.contains(&program) && !writes_output(program, &rest));
    let mut candidates: Vec<&str> = written.collect();
    if !reads_only {
        candidates.extend(operands.iter().map(String::as_str));
        // `sort -oFILE`: the output option carries its value in the same word.
        if matches!(program, Some("sort" | "less")) {
            candidates.extend(
                rest.iter()
                    .filter(|word| word.starts_with('-') && !word.starts_with("--"))
                    .filter_map(|word| word.split_once(['o', 'O']).map(|(_, value)| value))
                    .filter(|value| !value.is_empty()),
            );
        }
    }
    for word in candidates {
        match resolve_path(word, cx) {
            None => return Floor::Unresolved(word.to_owned()),
            Some(path) if protected_resolved(&path, cx) => {
                return Floor::Protected(word.to_owned())
            }
            Some(_) => {}
        }
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

    fn link(target: &Path, at: &Path) {
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    #[test]
    fn dot_dot_after_a_symlink_climbs_out_of_the_target() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(root.join("config/subdir")).unwrap();
        link(&root.join("config/subdir"), &f.cx.workspace.join("link"));
        // Spelled `project/config.json` once folded, but written to config/config.json.
        assert_eq!(
            resolve_path("link/../config.json", &f.cx),
            Some(root.join("config/config.json"))
        );
        assert!(matches!(
            check(&native_call("write", "link/../config.json"), &[], &f.cx),
            Floor::Protected(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, "echo x > link/../config.json"),
            Floor::Protected(_)
        ));
        // A `..` after plain directories still folds.
        assert_eq!(
            check(&native_call("write", "a/../b.txt"), &[], &f.cx),
            Floor::None
        );
    }

    #[test]
    fn a_symlinked_workspace_resolves_to_the_real_directory() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().to_path_buf();
        link(&f.cx.workspace, &root.join("work"));
        let cx = FloorContext {
            workspace: root.join("work"),
            ..f.cx.clone()
        };
        assert_eq!(
            resolve_path("src/a.rs", &cx),
            Some(root.join("project/src/a.rs"))
        );
        assert_eq!(
            resolve_path("../config/config.json", &cx),
            Some(root.join("config/config.json"))
        );
        assert!(matches!(
            check(&native_call("write", "../config/config.json"), &[], &cx),
            Floor::Protected(_)
        ));
    }

    #[test]
    fn resolution_errors_fail_closed() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().to_path_buf();
        link(&root.join("project/loop-b"), &root.join("project/loop-a"));
        link(&root.join("project/loop-a"), &root.join("project/loop-b"));
        assert_eq!(resolve_path("loop-a/x", &f.cx), None);
        assert_eq!(resolve_path("a\0b", &f.cx), None);
        for tool in ["read", "write"] {
            assert!(
                matches!(
                    check(&native_call(tool, "loop-a/x"), &[], &f.cx),
                    Floor::Unresolved(_)
                ),
                "{tool}"
            );
        }
        assert!(matches!(
            bash_floor(&f.cx, "touch loop-a/x"),
            Floor::Unresolved(_)
        ));
        assert!(protected_write(Path::new("loop-a/x"), &f.cx));
        // A dangling link is followed to where a write would create the file.
        link(
            &root.join("config/config.json"),
            &root.join("project/dangling"),
        );
        assert!(matches!(
            check(&native_call("write", "dangling"), &[], &f.cx),
            Floor::Protected(_)
        ));
    }

    #[test]
    fn symlinked_config_database_and_flows_keep_their_protection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["project", "config", "data", "elsewhere/flows", "home"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        std::fs::write(root.join("elsewhere/config.json"), "{}").unwrap();
        std::fs::write(root.join("elsewhere/withnav.db"), "").unwrap();
        link(
            &root.join("elsewhere/config.json"),
            &root.join("config/config.json"),
        );
        link(
            &root.join("elsewhere/withnav.db"),
            &root.join("data/withnav.db"),
        );
        link(&root.join("elsewhere/flows"), &root.join("config/flows"));
        let cx = FloorContext {
            workspace: root.join("project"),
            config_dir: Some(root.join("config")),
            data_dir: Some(root.join("data")),
            home: Some(root.join("home")),
            ..Default::default()
        };
        for path in [
            // The configured spellings.
            root.join("config/config.json"),
            root.join("data/withnav.db"),
            root.join("config/flows/mine.toml"),
            // Where the links lead.
            root.join("elsewhere/config.json"),
            root.join("elsewhere/withnav.db"),
            root.join("elsewhere/flows/mine.toml"),
        ] {
            let name = path.display().to_string();
            assert!(
                matches!(
                    check(&native_call("write", &name), &[], &cx),
                    Floor::Protected(_)
                ),
                "{name}"
            );
        }
        // A link in the workspace to the real file is caught too.
        link(&root.join("elsewhere/config.json"), &root.join("project/c"));
        assert!(matches!(
            check(&native_call("edit", "c"), &[], &cx),
            Floor::Protected(_)
        ));
        assert_eq!(check(&native_call("read", "c"), &[], &cx), Floor::None);
    }

    #[test]
    fn output_options_and_read_write_redirections_are_writes() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().display().to_string();
        let config = format!("{root}/config/config.json");
        for line in [
            format!("sort -o {config} in"),
            format!("sort -uo {config} in"),
            format!("sort -o{config} in"),
            format!("sort --output {config} in"),
            format!("sort --output={config} in"),
            format!("echo '{{}}' 1<> {config}"),
            format!("echo '{{}}' <> {config}"),
            format!("cat in > {config}"),
        ] {
            assert!(
                matches!(bash_floor(&f.cx, &line), Floor::Protected(_)),
                "{line}"
            );
        }
        for line in [
            format!("sort {config}"),
            format!("sort -u {config} -t,"),
            format!("cat < {config}"),
            format!("cat 0< {config}"),
            format!("grep x <<< {config}"),
        ] {
            assert_eq!(bash_floor(&f.cx, &line), Floor::None, "{line}");
        }
    }

    #[test]
    fn opaque_lines_keep_quoted_paths_whole() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().to_path_buf();
        let spaced = root.join("home/my keys");
        std::fs::create_dir_all(&spaced).unwrap();
        let cx = FloorContext {
            home: Some(root.join("home")),
            config_dir: Some(spaced.clone()),
            ..f.cx.clone()
        };
        for (line, credential_expected) in [
            ("if true; then cat \"my keys/.env\"; fi", true),
            ("if true; then cat 'a b/.ssh/id_ed25519'; fi", true),
            ("if true; then cat my\\ keys/.env; fi", true),
            ("if true; then cat \"a b\"/.env; fi", true),
        ] {
            assert!(split(line)[0].opaque, "{line}");
            assert_eq!(
                matches!(bash_floor(&cx, line), Floor::Credential(_)),
                credential_expected,
                "{line}"
            );
        }
        let line = format!(
            "if true; then echo x > \"{}/config.json\"; fi",
            spaced.display()
        );
        assert!(split(&line)[0].opaque);
        assert!(matches!(bash_floor(&cx, &line), Floor::Protected(_)));
        let line = format!(
            "for i in 1; do cp a '{}/config.json'; done",
            spaced.display()
        );
        assert!(matches!(bash_floor(&cx, &line), Floor::Protected(_)));
        assert_eq!(
            scan_words("a 'b c'\"d e\"f x\\ y;z"),
            ["a", "b cd ef", "x y", "z"]
        );
    }

    #[test]
    fn old_import_path_still_resolves() {
        assert!(crate::agent::permission_gate::sensitive_read(Path::new(
            "/x/auth.json"
        )));
        assert!(!crate::agent::permission_gate::SENSITIVE_NAMES.is_empty());
    }
}
