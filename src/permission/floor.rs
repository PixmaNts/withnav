//! The fixed floor: credentials and WithNav's own configuration and database.
//!
//! Only yolo skips it. Credentials ask for reads and writes alike; the
//! configuration, the database and the global flows directory are free to read
//! but need the permission flow's protected check before they are written.
//! For bash the floor is best effort: it looks at literal path words,
//! redirection targets and WithNav's own rule-changing commands. Only certain
//! writes are [`Floor::Protected`]; a command that merely names a protected
//! file is [`Floor::NamesProtected`] and the protected check decides whether
//! it only reads. It does not expand globs or variables, and it is not a
//! sandbox.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{collections::VecDeque, path::Component};

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
    /// Temporary directories: the system temp directory and the scratch
    /// directories. A native `write` or `edit` there runs without the flow.
    pub temp_roots: Vec<PathBuf>,
    /// Holds `config.json` and the global flows directory.
    pub config_dir: Option<PathBuf>,
    /// Holds `withnav.db` and its write-ahead files.
    pub data_dir: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

/// The system temporary directories: `std::env::temp_dir()` and, on Unix, `/tmp`.
pub fn system_temp_roots() -> Vec<PathBuf> {
    let mut roots = vec![std::env::temp_dir()];
    if cfg!(unix) && !roots.iter().any(|root| root == Path::new("/tmp")) {
        roots.push(PathBuf::from("/tmp"));
    }
    roots
}

impl FloorContext {
    /// The roots of a tool context with the configured config and data
    /// directories of this machine.
    pub fn from_environment(cx: &crate::tools::ToolContext) -> Self {
        Self {
            workspace: cx.workspace.clone(),
            writable_roots: cx.writable_roots.clone(),
            readable_roots: cx.readable_roots.clone(),
            temp_roots: system_temp_roots(),
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
    /// WithNav's own configuration or database is certainly written: a
    /// redirection or output option to it, a writer (`cp`, `rm`, `sed -i`, …)
    /// whose written operand it is, a SQLite dot-command that saves to it, or
    /// a `nav permissions` writer. The string names the target.
    Protected(String),
    /// A command that is not a known reader names a protected file without
    /// certainly writing it (`sqlite3 withnav.db …`, a script given the
    /// path). Whether it only reads or changes it is the permission flow's
    /// protected question, never a guess of the floor.
    NamesProtected(String),
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
    protected_word(&path.to_string_lossy(), cx).unwrap_or(true)
}

/// Whether writing the word changes a protected file; `None` when it cannot be
/// resolved. A write may land on the link itself or on what it points at, so
/// the word is protected when either the fully resolved target is, or the path
/// as requested (every directory resolved, the final component not followed)
/// is: a link below the flows directory pointing elsewhere is still a file of
/// the flows directory.
fn protected_word(word: &str, cx: &FloorContext) -> Option<bool> {
    let followed = resolve_path(word, cx)?;
    let requested = resolve(word, cx, false)?;
    Some(protected_resolved(&followed, cx) || protected_resolved(&requested, cx))
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
    #[cfg(windows)]
    {
        let identity = |path: &Path| crate::tools::nofollow::path_identity(path);
        identity(path).is_ok_and(|path| {
            files.iter().any(|file| {
                identity(file).is_ok_and(|file| {
                    crate::tools::nofollow::path_within(&path, &file)
                        && crate::tools::nofollow::path_within(&file, &path)
                })
            }) || roots.iter().any(|root| {
                identity(root).is_ok_and(|root| crate::tools::nofollow::path_within(&path, &root))
            })
        })
    }
    #[cfg(unix)]
    {
        files.iter().any(|file| path == file) || roots.iter().any(|root| path.starts_with(root))
    }
}

#[cfg(windows)]
fn windows_bash_spelling(path: &str, cx: &FloorContext) -> String {
    if path.starts_with("--") {
        if let Some((option, value)) = path.split_once('=') {
            return format!("{option}={}", windows_bash_spelling(value, cx));
        }
    } else if path.starts_with('-') {
        if let Some((at, _)) = path
            .char_indices()
            .skip(1)
            .find(|(_, c)| matches!(c, 'o' | 'O'))
        {
            if !path[at + 1..].is_empty() {
                return format!(
                    "{}{}",
                    &path[..at + 1],
                    windows_bash_spelling(&path[at + 1..], cx)
                );
            }
        }
    }
    let path = path.replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    let path = path.strip_prefix("/cygdrive").unwrap_or(path);
    let path = if path.to_ascii_uppercase().ends_with("::$DATA") {
        &path[..path.len() - 7]
    } else if path.to_ascii_uppercase().ends_with(":$DATA") {
        &path[..path.len() - 6]
    } else {
        path
    };
    if path.eq_ignore_ascii_case("%USERPROFILE%") {
        return cx
            .home
            .as_ref()
            .map_or(path.to_owned(), |home| home.display().to_string());
    }
    if path
        .get(..14)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("%USERPROFILE%/"))
    {
        return cx.home.as_ref().map_or(path.to_owned(), |home| {
            home.join(&path[14..]).display().to_string()
        });
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
        format!("{}:{}", &path[1..2], &path[2..])
    } else {
        path.to_owned()
    }
}

/// Links followed before a path counts as a loop, as the kernel does.
#[cfg(unix)]
const MAX_LINKS: usize = 40;

/// Resolve a path the way execution does: `~` is the home directory, a
/// relative path is anchored in the workspace, and links are followed one
/// component at a time, so a `..` after a symlink climbs out of the link's
/// target, not out of its spelling. A dangling link is followed to where a
/// write would create the file. `None` when resolution fails (a link loop, an
/// unreadable directory, a NUL byte); callers fail closed.
pub fn resolve_path(path: &str, cx: &FloorContext) -> Option<PathBuf> {
    resolve(path, cx, true)
}

/// [`resolve_path`], optionally leaving a link in the final component alone.
#[cfg(windows)]
fn resolve(path: &str, cx: &FloorContext, _follow_last: bool) -> Option<PathBuf> {
    crate::tools::nofollow::resolve_target(Path::new(path), &cx.workspace, cx.home.as_deref()).ok()
}

#[cfg(unix)]
fn resolve(path: &str, cx: &FloorContext, follow_last: bool) -> Option<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    let expanded: Option<PathBuf> = None;
    let joined = if let Some(expanded) = expanded {
        expanded
    } else if path == "~" {
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
                    Ok(meta)
                        if meta.file_type().is_symlink() && (follow_last || !queue.is_empty()) =>
                    {
                        links += 1;
                        if links > MAX_LINKS {
                            return None;
                        }
                        let target = std::fs::read_link(&resolved).ok()?;
                        resolved.pop();
                        push_all(&mut queue, &target);
                    }
                    Ok(_) => {}
                    // No file can have this name: it does not exist yet, it continues
                    // below a file, or a component is longer than a file name may be
                    // (an opaque line's text, such as a heredoc body, scanned as one
                    // word). Such a word names nothing, so it is never a protected file.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::NotADirectory
                                | std::io::ErrorKind::InvalidFilename
                        ) =>
                    {
                        missing = true
                    }
                    Err(_) => return None,
                }
            }
            Component::RootDir => {
                resolved.push(&part);
            }
            Component::Prefix(_) => {
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
                Floor::NamesProtected(_) => 3,
                Floor::None => 4,
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
        #[cfg(windows)]
        let path =
            resolve_path(path, cx).map_or(path.to_owned(), |path| path.display().to_string());
        return Floor::Credential(path.to_owned());
    }
    match (resolve_path(path, cx), writes) {
        (None, _) => Floor::Unresolved(path.to_owned()),
        (Some(_resolved), true) if protected_word(path, cx) == Some(true) => {
            #[cfg(windows)]
            let path = _resolved.display().to_string();
            Floor::Protected(path.to_owned())
        }
        _ => Floor::None,
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
/// [`output_targets`].
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

/// Files a command is told to write through an output option: attached
/// (`-oPATH`, `--output=PATH`), separate (`-o PATH`, `--output PATH`) or in a
/// short cluster (`-uo PATH`). `sort` and `less` have their own options; other
/// readers have none; any other program, and an opaque line that cannot be
/// attributed to a program, gets the common `-o`, `-O`, `--output` and
/// `--log-file` (best effort, nothing is expanded).
fn output_targets<'a>(program: Option<&str>, words: &'a [String], opaque: bool) -> Vec<&'a str> {
    let (short, long): (&[char], &[&str]) = match program {
        Some("sort") => (&['o'], &["output"]),
        Some("less") => (&['o', 'O'], &["log-file", "LOG-FILE"]),
        Some(program) if READERS.contains(&program) => return Vec::new(),
        _ => (&['o', 'O'], &["output", "log-file"]),
    };
    let mut targets = Vec::new();
    let mut iter = words.iter();
    while let Some(word) = iter.next() {
        if word == "--" && !opaque {
            break;
        }
        if let Some(option) = word.strip_prefix("--") {
            for name in long {
                if option == *name {
                    targets.extend(iter.clone().next().map(String::as_str));
                } else if let Some(value) = option
                    .strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('='))
                {
                    targets.push(value);
                }
            }
        } else if word.starts_with('-') {
            if let Some((at, _)) = word.char_indices().skip(1).find(|(_, c)| short.contains(c)) {
                match &word[at + 1..] {
                    "" => targets.extend(iter.clone().next().map(String::as_str)),
                    value => targets.push(value),
                }
            }
        }
    }
    targets
}

fn basename(word: &str) -> &str {
    #[cfg(windows)]
    {
        word.rsplit(['/', '\\']).next().unwrap_or(word)
    }
    #[cfg(unix)]
    {
        word.rsplit('/').next().unwrap_or(word)
    }
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
    #[cfg(windows)]
    let normalized = {
        let mut normalized = command.clone();
        normalized.words = source_words(command)
            .iter()
            .map(|word| windows_bash_spelling(word, cx))
            .collect();
        for redirect in &mut normalized.redirections {
            redirect.target = windows_bash_spelling(&redirect.target, cx);
        }
        normalized
    };
    #[cfg(windows)]
    let command = &normalized;
    #[cfg(windows)]
    let all = command.words.clone();
    #[cfg(unix)]
    let all = source_words(command);
    let (program, rest, operands) = operands(command, &all);
    let program = program.as_deref().map(basename);
    // Every literal file the command names or writes, found before any check
    // so that the credential, resolution and protected checks see all of them.
    let outputs = output_targets(program, &rest, command.opaque);
    let redirected = command
        .redirections
        .iter()
        .map(|redirection| redirection.target.as_str());
    if let Some(word) = operands
        .iter()
        .map(String::as_str)
        .chain(redirected)
        .chain(outputs.iter().copied())
        .find(|word| names_credential(word, cx))
    {
        return Floor::Credential(word.to_owned());
    }
    let reads_only =
        program.is_some_and(|program| READERS.contains(&program)) && outputs.is_empty();
    // A known writer: what it writes is known, its other operands are read.
    // An opaque line that names a writer or holds a `>` anywhere writes every
    // operand.
    let writer = match program {
        Some(_) => written_operands(program, &rest, &operands),
        None if command.opaque
            && (command.raw.contains('>')
                || all
                    .iter()
                    .any(|word| WRITERS.contains(&basename(word)) || basename(word) == "sed")) =>
        {
            Some(operands.iter().map(String::as_str).collect())
        }
        None => None,
    };
    // Certain writes: redirections, output options, the written operands of a
    // known writer, and the files SQL text saves to.
    let written: Vec<&str> = command
        .redirections
        .iter()
        .filter(|redirection| !reads_only_redirection(&redirection.op))
        .map(|redirection| redirection.target.as_str())
        .chain(outputs.iter().copied())
        .chain(writer.iter().flatten().copied())
        .chain(all.iter().flat_map(|word| sql_written_files(word)))
        .collect();
    for word in written {
        if not_a_windows_path(word, cx) {
            continue;
        }
        match protected_word(word, cx) {
            None => return Floor::Unresolved(word.to_owned()),
            Some(true) => return Floor::Protected(word.to_owned()),
            Some(false) => {}
        }
    }
    if !reads_only {
        // Any other operand: a path that cannot be resolved stays unknown.
        for word in &operands {
            if not_a_windows_path(word, cx) {
                continue;
            }
            if protected_word(word, cx).is_none() {
                return Floor::Unresolved(word.clone());
            }
        }
    }
    let words = || all.iter().map(|word| basename(word));
    let words: Vec<&str> = words().collect();
    let nav_writer = words.windows(2).position(|pair| {
        pair[0] == "permissions"
            && matches!(pair[1], "add" | "revoke" | "validate" | "revoke-validation")
    });
    // `--help` or `-h` after the subcommand prints its help and changes nothing.
    let help = nav_writer.is_some_and(|at| {
        words[at + 2..]
            .iter()
            .take_while(|word| **word != "--")
            .any(|word| matches!(*word, "--help" | "-h"))
    });
    if words.contains(&"nav") && nav_writer.is_some() && !help {
        return Floor::Protected("nav permissions".into());
    }
    if !reads_only && writer.is_none() {
        // Naming a protected file anywhere in the command, inside a quoted
        // script or SQL text too, leaves the question to the flow.
        if let Some(name) = all.iter().flat_map(|word| path_tokens(word)).find(|token| {
            basename(token).starts_with("withnav.db") || protected_word(token, cx) == Some(true)
        }) {
            return Floor::NamesProtected(name.to_owned());
        }
    }
    Floor::None
}

/// Shell operands also include URLs, header values and arbitrary text. On
/// Windows, a word that is not a valid Win32 filename cannot name a protected
/// file; native paths still fail closed through their own resolver.
#[cfg(windows)]
fn not_a_windows_path(word: &str, cx: &FloorContext) -> bool {
    if let Ok(path) =
        crate::tools::nofollow::requested_path(Path::new(word), &cx.workspace, cx.home.as_deref())
    {
        if matches!(crate::tools::nofollow::path_identity(&path), Err(crate::error::ToolError::Io(ref error)) if matches!(error.raw_os_error(), Some(123 | 206)))
        {
            return true;
        }
        if word.contains(':') && !Path::new(word).is_absolute() {
            return true;
        }
    }
    false
}

#[cfg(not(windows))]
fn not_a_windows_path(_word: &str, _cx: &FloorContext) -> bool {
    false
}

/// The operands a known writer changes: every operand of `rm`, `mv`, `tee`,
/// `truncate` and the like, the destination of `cp`, `install` and `ln` (the
/// last operand, or the `-t` directory), `of=` of `dd`, and every operand of
/// `sed -i`. Best effort: nothing is expanded.
/// `None` when the program is not a known writer.
fn written_operands<'a>(
    program: Option<&str>,
    rest: &'a [String],
    operands: &'a [String],
) -> Option<Vec<&'a str>> {
    let all = || Some(operands.iter().map(String::as_str).collect());
    match program {
        Some(program)
            if WRITERS.contains(&program) && !matches!(program, "cp" | "install" | "ln" | "dd") =>
        {
            all()
        }
        Some("sed")
            if rest.iter().any(|word| {
                word == "--in-place"
                    || word.starts_with("--in-place=")
                    || (word.starts_with('-') && !word.starts_with("--") && word.contains('i'))
            }) =>
        {
            all()
        }
        Some("cp" | "install" | "ln") => {
            let mut iter = rest.iter();
            while let Some(word) = iter.next() {
                if word == "-t" || word == "--target-directory" {
                    return Some(iter.next().map(String::as_str).into_iter().collect());
                }
                if let Some(dir) = word
                    .strip_prefix("--target-directory=")
                    .or_else(|| word.strip_prefix("-t").filter(|d| !d.is_empty()))
                {
                    return Some(vec![dir]);
                }
            }
            Some(match operands {
                [_, .., last] => vec![last.as_str()],
                _ => Vec::new(),
            })
        }
        Some("dd") => Some(
            rest.iter()
                .filter_map(|word| word.strip_prefix("of="))
                .collect(),
        ),
        _ => None,
    }
}

/// Programs whose written operands [`written_operands`] knows (`sed` only with `-i`).
const WRITERS: &[&str] = &[
    "rm", "rmdir", "unlink", "shred", "mv", "tee", "truncate", "touch", "mkdir", "chmod", "chown",
    "chgrp", "cp", "install", "ln", "dd",
];

/// Files SQL or SQLite dot-command text inside one word saves to:
/// `.save F`, `.output F`, `.once F`, `.backup [DB] F` and `VACUUM INTO 'F'`.
fn sql_written_files(word: &str) -> Vec<&str> {
    let mut files = Vec::new();
    for statement in word.split(['\n', ';']) {
        let tokens: Vec<&str> = statement
            .split_whitespace()
            .map(|token| token.trim_matches(|c| c == '\'' || c == '"'))
            .collect();
        match tokens.first().map(|t| t.to_ascii_lowercase()).as_deref() {
            Some(".save" | ".output" | ".once" | ".backup") if tokens.len() > 1 => {
                files.extend(tokens.last().copied());
            }
            _ => {}
        }
        if let Some(at) = tokens.windows(2).position(|pair| {
            pair[0].eq_ignore_ascii_case("vacuum") && pair[1].eq_ignore_ascii_case("into")
        }) {
            files.extend(tokens.get(at + 2).copied());
        }
    }
    files.retain(|file| !file.is_empty());
    files
}

/// Path-like pieces of a word, a quoted script or SQL text included: split on
/// whitespace, quotes, brackets, `=`, `,` and `;`.
fn path_tokens(word: &str) -> impl Iterator<Item = &str> {
    word.split(|c: char| c.is_whitespace() || "'\"()[]{}=,;`".contains(c))
        .filter(|token| !token.is_empty())
}

/// Where a call acts, from its literal paths: the project, one of WithNav's
/// scratch directories, the system temp directory outside them, elsewhere, or
/// unknown (an opaque line, a path that cannot be resolved, a relative path
/// after a `cd`). For bash, the farthest place any literal operand or
/// redirection target names; a command naming no path acts in the project,
/// its working directory. `None` for a tool without a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Location {
    Project,
    Scratch,
    Temp,
    Outside,
    Unknown,
}

impl Location {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Scratch => "scratch",
            Self::Temp => "temp",
            Self::Outside => "outside",
            Self::Unknown => "unknown",
        }
    }
}

pub fn location(
    call: &ToolCall,
    commands: &[ShellCommand],
    cx: &FloorContext,
    scratch: &[PathBuf],
) -> Option<Location> {
    let roots = |paths: &[PathBuf]| -> Vec<PathBuf> {
        paths
            .iter()
            .filter_map(|root| resolve_path(&root.to_string_lossy(), cx))
            .collect()
    };
    let (scratch, temp) = (roots(scratch), roots(&cx.temp_roots));
    let project = roots(
        &std::iter::once(cx.workspace.clone())
            .chain(cx.writable_roots.iter().cloned())
            .collect::<Vec<_>>(),
    );
    let place = |word: &str| match resolve_path(word, cx) {
        None => Location::Unknown,
        Some(path) if scratch.iter().any(|root| path.starts_with(root)) => Location::Scratch,
        Some(path) if project.iter().any(|root| path.starts_with(root)) => Location::Project,
        Some(path) if temp.iter().any(|root| path.starts_with(root)) => Location::Temp,
        Some(_) => Location::Outside,
    };
    match call.name.as_str() {
        "read" | "grep" | "glob" | "write" | "edit" => Some(
            call.args
                .get("path")
                .and_then(Value::as_str)
                .map_or(Location::Project, place),
        ),
        "bash" => {
            let mut farthest = Location::Project;
            // After a `cd`, a relative path no longer starts in the project.
            let mut moved = false;
            for command in commands {
                if command.opaque {
                    return Some(Location::Unknown);
                }
                let (program, _, operands) = operands(command, &command.words);
                let program = program.as_deref().map(basename);
                let targets = command
                    .redirections
                    .iter()
                    .map(|redirection| redirection.target.as_str())
                    .filter(|target| {
                        !target.starts_with("/dev/")
                            && !target.chars().all(|c| c.is_ascii_digit() || c == '-')
                    });
                for word in operands.iter().map(String::as_str).chain(targets) {
                    let absolute = word.starts_with('/') || word.starts_with('~');
                    let here = if moved && !absolute {
                        Location::Unknown
                    } else {
                        place(word)
                    };
                    farthest = farthest.max(here);
                }
                moved |= matches!(program, Some("cd" | "pushd" | "popd"));
            }
            Some(farthest)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::shell::split;
    use serde_json::json;

    #[cfg(windows)]
    #[test]
    fn windows_real_aliases_have_the_same_floor_and_dispatch_target() {
        let f = fixture();
        for relative in [
            "config/config.json",
            "data/withnav.db",
            "data/withnav.db-wal",
            "data/withnav.db-shm",
            "config/flows/custom.toml",
            "config/auth.json",
        ] {
            let target = f.cx.workspace.parent().unwrap().join(relative);
            std::fs::write(&target, "original").unwrap();
            let canonical = crate::tools::nofollow::path_identity(&target).unwrap();
            let mut spellings = vec![
                format!("{}.", target.display()),
                format!("{} ", target.display()),
                target.parent().unwrap().display().to_string()
                    + ".\\"
                    + &target.file_name().unwrap().to_string_lossy(),
                target.parent().unwrap().display().to_string()
                    + " \\"
                    + &target.file_name().unwrap().to_string_lossy(),
            ];
            if relative.contains("flows/") {
                spellings.push(target.display().to_string().replace("flows\\", "flows.\\"));
                spellings.push(target.display().to_string().replace("flows\\", "flows \\"));
            }
            if let Some(alias) = crate::tools::nofollow::short_alias(target.parent().unwrap()) {
                spellings.push(
                    alias
                        .join(target.file_name().unwrap())
                        .display()
                        .to_string(),
                );
            }
            for spelling in spellings {
                let floor = check(&native_call("write", &spelling), &[], &f.cx);
                assert!(
                    matches!(floor, Floor::Credential(_) | Floor::Protected(_)),
                    "{spelling}: {floor:?}"
                );
                let dispatch = crate::tools::validate_path(
                    &[f.cx.workspace.parent().unwrap().to_path_buf()],
                    Path::new(&spelling),
                )
                .unwrap();
                assert_eq!(dispatch, canonical, "{spelling}");
                crate::tools::nofollow::write_atomic(&dispatch, b"updated").unwrap();
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "updated");
                for suffix in [":$DATA", "::$DATA"] {
                    assert!(matches!(
                        bash_floor(&f.cx, &format!("echo x > '{}{suffix}'", target.display())),
                        Floor::Credential(_) | Floor::Protected(_)
                    ));
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn long_descendant_of_global_flows_is_protected() {
        let f = fixture();
        let root = f.cx.config_dir.as_ref().unwrap().join("flows");
        let target = root
            .join("a".repeat(110))
            .join("b".repeat(110))
            .join("file.toml");
        let resolved = crate::tools::canonical_path(&target).unwrap();
        assert!(resolved.to_string_lossy().starts_with(r"\\?\"));
        crate::tools::nofollow::write_atomic(&resolved, b"flow").unwrap();
        assert!(protected_resolved(&resolved, &f.cx));
        assert!(
            matches!(check(&native_call("write", &target.to_string_lossy()), &[], &f.cx),
            Floor::Protected(path) if path == resolved.display().to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_literal_floor_spellings() {
        let cx = FloorContext {
            workspace: PathBuf::from(r"C:\Users\Captain\project"),
            config_dir: Some(PathBuf::from(r"C:\Users\Captain\config")),
            data_dir: Some(PathBuf::from(r"C:\Users\Captain\data")),
            home: Some(PathBuf::from(r"C:\Users\Captain")),
            ..Default::default()
        };
        for path in [
            r"C:\Users\Captain\config\config.json",
            "c:/users/captain/CONFIG/CONFIG.JSON",
            r"\\?\C:\Users\Captain\config\config.json",
            "/c/Users/Captain/config/config.json",
            "/cygdrive/c/Users/Captain/config/config.json",
            r"~\config\config.json",
            r"%USERPROFILE%\config\config.json",
            "C:/Users/Captain/data/withnav.db-wal",
            "C:/Users/Captain/data/withnav.db",
            "C:/Users/Captain/data/withnav.db-shm",
            "C:/Users/Captain/config/flows/custom.toml",
        ] {
            assert!(
                protected_word(&windows_bash_spelling(path, &cx), &cx).unwrap(),
                "{path}"
            );
            for line in [
                format!("echo x > '{path}'"),
                format!("sort --output='{path}' in"),
                format!("if true; then cp a '{path}'; fi"),
            ] {
                assert!(
                    matches!(bash_floor(&cx, &line), Floor::Protected(_)),
                    "{line}"
                );
            }
        }
        for path in [
            r"C:\Users\Captain\AUTH.JSON",
            r"~\.ssh\id_custom",
            r"%USERPROFILE%\.ssh\id_custom",
            "/c/Users/Captain/.env",
            r"\\?\C:\Users\Captain\secret.KEY",
        ] {
            assert!(
                names_credential(&windows_bash_spelling(path, &cx), &cx),
                "{path}"
            );
            assert!(matches!(
                bash_floor(&cx, &format!("cat '{path}'")),
                Floor::Credential(_)
            ));
        }
        assert_eq!(basename(r"C:\bin\cat"), "cat");
    }

    #[cfg(windows)]
    #[test]
    fn bash_mount_spellings_protect_real_temp_targets() {
        let f = fixture();
        let target = f.cx.config_dir.as_ref().unwrap().join("config.json");
        std::fs::write(&target, "{}").unwrap();
        let plain =
            crate::tools::relative_path_text(&crate::tools::canonical_path(&target).unwrap());
        let mounted = format!("/{}{}", plain[..1].to_ascii_lowercase(), &plain[2..]);
        for spelling in [plain, mounted] {
            for line in [
                format!("echo x > '{spelling}'"),
                format!("sort -o'{spelling}' in"),
                format!("if true; then cp a '{spelling}'; fi"),
            ] {
                assert!(
                    matches!(bash_floor(&f.cx, &line), Floor::Protected(_)),
                    "{line}"
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_root_relative_paths_match_dispatch_not_bash_drive_mapping() {
        let f = fixture();
        let target = f.cx.config_dir.as_ref().unwrap().join("config.json");
        std::fs::write(&target, "protected").unwrap();
        let target = crate::tools::nofollow::path_identity(&target).unwrap();
        let root = crate::tools::nofollow::drive_root(&target).unwrap();
        let tail = target
            .strip_prefix(&root)
            .unwrap()
            .display()
            .to_string()
            .replace('\\', "/");
        let cx = crate::tools::ToolContext {
            binding: None,
            workspace: f.cx.workspace.clone(),
            writable_roots: vec![],
            readable_roots: vec![],
        };
        // No second drive is needed: native /d is an ordinary root-relative
        // directory, even when Git Bash would interpret it as a drive mount.
        for spelling in [
            format!("/d/../{tail}"),
            format!("/cygdrive/d/../../{tail}"),
            format!("/{tail}"),
        ] {
            let dispatch =
                crate::agent::permissions::resolve_read_target(Path::new(&spelling), &cx).unwrap();
            assert_eq!(dispatch, target, "{spelling}");
            assert_eq!(resolve_path(&spelling, &f.cx), Some(dispatch));
            assert!(
                matches!(
                    check(&native_call("write", &spelling), &[], &f.cx),
                    Floor::Protected(_)
                ),
                "{spelling}"
            );
        }
        let stream = format!("{}::$DATA", target.display());
        assert!(crate::agent::permissions::resolve_read_target(Path::new(&stream), &cx).is_err());
        assert!(matches!(
            check(&native_call("write", &stream), &[], &f.cx),
            Floor::Unresolved(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, &format!("echo x > '{stream}'")),
            Floor::Protected(_)
        ));
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        cx: FloorContext,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::tools::canonical_path(dir.path()).unwrap();
        for sub in ["project", "config/flows", "data", "home/.ssh"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let cx = FloorContext {
            workspace: root.join("project"),
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
            temp_roots: Vec::new(),
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
            #[cfg(windows)]
            let protected_name = crate::tools::canonical_path(&full)
                .unwrap()
                .display()
                .to_string();
            #[cfg(unix)]
            let protected_name = name.clone();
            assert_eq!(
                check(&native_call("write", &name), &[], &f.cx),
                Floor::Protected(protected_name.clone())
            );
            assert_eq!(
                check(&native_call("edit", &name), &[], &f.cx),
                Floor::Protected(protected_name)
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

    #[cfg(unix)]
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
        let root = crate::tools::relative_path_text(f.cx.workspace.parent().unwrap());
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
            bash_floor(&f.cx, &format!("echo x > \"{root}/config/config.json\"")),
            Floor::Protected(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, &format!("cp a \"{root}/data/withnav.db\"")),
            Floor::Protected(_)
        ));
        // Reading is free.
        assert_eq!(
            bash_floor(&f.cx, &format!("cat \"{root}/config/config.json\"")),
            Floor::None
        );
        assert_eq!(
            bash_floor(&f.cx, &format!("sqlite3 \"{root}/data/other.db\" .tables")),
            Floor::None
        );
        // A program that merely names WithNav's database may read or change
        // it: the permission flow's protected question decides, never the
        // floor alone. `-readonly` is not special.
        for line in [
            format!("sqlite3 \"{root}/data/withnav.db\" 'drop table x'"),
            format!(
                "cd /tmp && sqlite3 -readonly -separator ' | ' \"{root}/data/withnav.db\" \"SELECT id FROM permission_decisions\""
            ),
            format!("sqlite3 \"{root}/data/withnav.db\" \"SELECT '-readonly'\""),
            format!(
                "S=$(sqlite3 -readonly \"{root}/data/withnav.db\" \"SELECT id FROM permission_decisions WHERE id=69\"); echo \"$S\""
            ),
            format!("echo \"$(sqlite3 -readonly '{root}/data/withnav.db' 'SELECT 1')\""),
            format!("S=$(sqlite3 \"{root}/data/withnav.db\" \"drop table x\")"),
            format!("litecli \"{root}/data/withnav.db\""),
            format!("python3 inspect.py \"{root}/data/withnav.db\""),
            format!("python3 inspect.py --db=\"{root}/config/config.json\""),
            // Relative to a directory the floor does not follow: the name is enough.
            "cd ~/.local/share/withnav && sqlite3 withnav.db 'DELETE FROM permission_rules'".to_owned(),
        ] {
            assert!(
                matches!(bash_floor(&f.cx, &line), Floor::NamesProtected(_)),
                "{line}: {:?}",
                bash_floor(&f.cx, &line)
            );
        }
        // A dot-command that writes a file names it inside the SQL text: a
        // certain write.
        for line in [
            format!("sqlite3 -readonly /tmp/other.db \".save '{root}/data/withnav.db'\""),
            format!("S=$(sqlite3 -readonly /tmp/x.db \".save '{root}/data/withnav.db'\")"),
            format!("sqlite3 /tmp/x.db \".output '{root}/config/config.json'\" 'select 1'"),
            format!("echo $(sqlite3 -readonly /tmp/x.db) > \"{root}/data/withnav.db\""),
        ] {
            assert!(
                matches!(bash_floor(&f.cx, &line), Floor::Protected(_)),
                "{line}: {:?}",
                bash_floor(&f.cx, &line)
            );
        }
        // Writers whose operand is a protected file write it.
        for line in [
            format!("mv a \"{root}/data/withnav.db\""),
            format!("rm -f \"{root}/data/withnav.db-wal\""),
            format!("tee -a \"{root}/config/config.json\""),
            format!("dd if=/dev/zero of=\"{root}/data/withnav.db\" bs=1 count=1"),
            format!("install -m 600 a \"{root}/config/config.json\""),
            format!("ln -sf /tmp/x \"{root}/config/config.json\""),
            format!("truncate -s 0 \"{root}/data/withnav.db\""),
            format!("sed -i 's/a/b/' \"{root}/config/config.json\""),
            format!("cp -t \"{root}/config/flows\" a.toml"),
        ] {
            assert!(
                matches!(bash_floor(&f.cx, &line), Floor::Protected(_)),
                "{line}: {:?}",
                bash_floor(&f.cx, &line)
            );
        }
        // Copying from a protected file, or linking to it, only reads it.
        for line in [
            format!("cp \"{root}/data/withnav.db\" /tmp/backup.db"),
            format!("ln -s \"{root}/config/config.json\" /tmp/c.json"),
            format!("sqlite3 \"{root}/data/other.db\" .tables"),
        ] {
            assert_eq!(bash_floor(&f.cx, &line), Floor::None, "{line}");
        }
        assert!(matches!(
            bash_floor(&f.cx, "nav permissions add bash '*' allow"),
            Floor::Protected(_)
        ));
        assert!(matches!(
            bash_floor(&f.cx, "sudo nav permissions revoke 3"),
            Floor::Protected(_)
        ));
        assert_eq!(bash_floor(&f.cx, "nav permissions list"), Floor::None);
        // Help changes nothing.
        for line in [
            "nav permissions --help; nav permissions add --help",
            "target/release/nav permissions revoke -h",
            "nav permissions validate --help | head -30",
        ] {
            assert_eq!(bash_floor(&f.cx, line), Floor::None, "{line}");
        }
        assert!(matches!(
            bash_floor(&f.cx, "nav permissions add bash 'x --help' allow"),
            Floor::Protected(_)
        ));
    }

    #[test]
    fn credentials_win_over_protected_and_opaque_lines_are_scanned() {
        let f = fixture();
        let root = crate::tools::relative_path_text(f.cx.workspace.parent().unwrap());
        assert!(matches!(
            bash_floor(
                &f.cx,
                &format!("cp \"{root}/config/config.json\" x; cat .env")
            ),
            Floor::Credential(_)
        ));
        let opaque = "if true; then cat .env; fi";
        assert!(split(opaque)[0].opaque);
        assert!(matches!(bash_floor(&f.cx, opaque), Floor::Credential(_)));
    }

    #[cfg(unix)]
    fn link(target: &Path, at: &Path) {
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn symlinked_config_database_and_flows_keep_their_protection() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::tools::canonical_path(dir.path()).unwrap();
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
        // The relative operand is space-free even when TEMP contains spaces.
        assert!(matches!(
            bash_floor(&f.cx, "sort -o ../config/config.json in"),
            Floor::Protected(_)
        ));
        let root = crate::tools::relative_path_text(f.cx.workspace.parent().unwrap());
        let config = format!("\"{root}/config/config.json\"");
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
    #[cfg(unix)]
    fn words_that_cannot_name_a_file_are_not_unresolved() {
        let f = fixture();
        // A heredoc makes the line opaque; its body is scanned as words, one of
        // them longer than any file name.
        let body = "x".repeat(300);
        let line =
            format!("python3 - <<'EOF'\ns = \"\"\"{body}\"\"\"\nopen('a.rs','w').write(s)\nEOF");
        assert!(split(&line)[0].opaque);
        assert_eq!(bash_floor(&f.cx, &line), Floor::None);
        // Below a regular file nothing can exist.
        std::fs::write(f.cx.workspace.join("a.rs"), "").unwrap();
        assert_eq!(bash_floor(&f.cx, "rm a.rs/x"), Floor::None);
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
    fn attached_and_separate_output_targets_meet_every_check() {
        let f = fixture();
        let root = crate::tools::relative_path_text(f.cx.workspace.parent().unwrap());
        let config = format!("\"{root}/config/config.json\"");
        for line in [
            "sort -o.env input".to_owned(),
            "sort -o .env input".to_owned(),
            "sort --output=.env input".to_owned(),
            "sort --output .env input".to_owned(),
            "sort -uo .env input".to_owned(),
            "gcc -o.env main.c".to_owned(),
            "if true; then sort -o.env input; fi".to_owned(),
            "if true; then sort -o .env input; fi".to_owned(),
            "if true; then sort --output=.env input; fi".to_owned(),
            "if true; then sort --output .env input; fi".to_owned(),
        ] {
            assert!(
                matches!(bash_floor(&f.cx, &line), Floor::Credential(_)),
                "{line}"
            );
        }
        for line in [
            format!("sort -o{config} input"),
            format!("sort -o {config} input"),
            format!("if true; then sort -o{config} input; fi"),
            format!("if true; then sort -o {config} input; fi"),
            format!("if true; then sort --output={config} input; fi"),
            format!("if true; then sort --output {config} input; fi"),
            format!("if true; then cat in > {config}; fi"),
        ] {
            assert!(
                matches!(bash_floor(&f.cx, &line), Floor::Protected(_)),
                "{line}"
            );
        }
        #[cfg(unix)]
        {
            // An output that cannot be resolved fails closed, attached or not.
            let project = &f.cx.workspace;
            link(&project.join("b"), &project.join("a"));
            link(&project.join("a"), &project.join("b"));
            for line in [
                "sort -oa/x input",
                "sort -o a/x input",
                "if true; then sort -oa/x i; fi",
            ] {
                assert!(
                    matches!(bash_floor(&f.cx, line), Floor::Unresolved(_)),
                    "{line}"
                );
            }
        }
        // Reading and an ordinary output stay free.
        assert_eq!(bash_floor(&f.cx, "sort -o out.txt input"), Floor::None);
        assert_eq!(
            bash_floor(&f.cx, "if true; then sort -u input; fi"),
            Floor::None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_below_the_flows_directory_stays_a_flows_file() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("elsewhere/custom.toml"), "").unwrap();
        let inside = root.join("config/flows/custom.toml");
        link(&root.join("elsewhere/custom.toml"), &inside);
        // A dangling one too: the write would create the target.
        let dangling = root.join("config/flows/new.toml");
        link(&root.join("elsewhere/new.toml"), &dangling);
        for path in [&inside, &dangling] {
            let name = path.display().to_string();
            assert!(protected_write(path, &f.cx), "{name}");
            for tool in ["write", "edit"] {
                assert!(
                    matches!(
                        check(&native_call(tool, &name), &[], &f.cx),
                        Floor::Protected(_)
                    ),
                    "{tool} {name}"
                );
            }
            assert_eq!(check(&native_call("read", &name), &[], &f.cx), Floor::None);
            for line in [
                format!("echo x > {name}"),
                format!("cp a {name}"),
                format!("sort -o {name} in"),
                format!("if true; then sort -o{name} in; fi"),
            ] {
                assert!(
                    matches!(bash_floor(&f.cx, &line), Floor::Protected(_)),
                    "{line}"
                );
            }
        }
        // Reached through a link to the flows directory itself.
        link(&root.join("config/flows"), &root.join("project/f"));
        assert!(matches!(
            check(&native_call("write", "f/custom.toml"), &[], &f.cx),
            Floor::Protected(_)
        ));
        // The target alone, outside, is not protected.
        assert!(!protected_write(&root.join("elsewhere/custom.toml"), &f.cx));
    }

    #[test]
    fn a_call_is_located_by_its_literal_paths() {
        let f = fixture();
        let root = f.cx.workspace.parent().unwrap().to_path_buf();
        for sub in ["tmp/withnav", "tmp/other"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let cx = FloorContext {
            temp_roots: vec![root.join("tmp"), root.join("tmp/withnav")],
            ..f.cx.clone()
        };
        let scratch = [root.join("tmp/withnav")];
        let at = |line: &str| {
            let call = ToolCall {
                id: "c".into(),
                name: "bash".into(),
                args: json!({"command": line}),
                args_valid: true,
            };
            location(&call, &split(line), &cx, &scratch)
        };
        let r = root.display();
        assert_eq!(at("cargo test"), Some(Location::Project));
        assert_eq!(at("rm -v src/a.rs"), Some(Location::Project));
        assert_eq!(
            at(&format!(
                "ls -la \"{r}/tmp/withnav/u.txt\" && rm -v \"{r}/tmp/withnav/u.txt\""
            )),
            Some(Location::Scratch)
        );
        assert_eq!(
            at(&format!("rm \"{r}/tmp/other/lock\"")),
            Some(Location::Temp)
        );
        assert_eq!(
            at(&format!(
                "echo x > \"{r}/tmp/withnav/a\" && rm \"{r}/home/x.txt\""
            )),
            Some(Location::Outside)
        );
        assert_eq!(at("cd /x && rm y"), Some(Location::Unknown));
        assert_eq!(at("if true; then rm a; fi"), Some(Location::Unknown));
        let file = |name: &str, path: &str| location(&native_call(name, path), &[], &cx, &scratch);
        assert_eq!(file("write", "src/a.rs"), Some(Location::Project));
        assert_eq!(
            file("write", &format!("{r}/tmp/withnav/n.md")),
            Some(Location::Scratch)
        );
        assert_eq!(
            file("read", &format!("{r}/home/n.md")),
            Some(Location::Outside)
        );
        let other = ToolCall {
            id: "c".into(),
            name: "fetch".into(),
            args: json!({"url": "https://example.com"}),
            args_valid: true,
        };
        assert_eq!(location(&other, &[], &cx, &scratch), None);
    }

    #[test]
    fn the_sensitive_name_helpers_recognise_a_credential_file() {
        assert!(crate::permission::floor::sensitive_read(Path::new(
            "/x/auth.json"
        )));
        assert!(!crate::permission::floor::SENSITIVE_NAMES.is_empty());
    }
}
