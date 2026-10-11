//! Deterministic shell risk floors; ordinary effects belong to Instinct.
use serde_json::Value;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

fn path_directory(directory: PathBuf, facts: &Value) -> Option<PathBuf> {
    if directory.is_absolute() {
        Some(directory)
    } else {
        Some(Path::new(facts["workspace"].as_str()?).join(directory))
    }
}

/// Resolve the actual PATH executable, not a script borrowing a tool's basename.
/// Both its spelling and symlink target must be outside every writable root,
/// with non-writable directory chains (secure current-user shim links are allowed).
pub(super) fn trusted_program(word: &str, facts: &Value) -> Option<(String, PathBuf)> {
    let name = basename(word);
    let path = Path::new(word);
    if word.contains('/') && !path.is_absolute() {
        return None;
    }
    let mut roots: Vec<PathBuf> = facts["writable_roots"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .collect();
    roots.push(PathBuf::from(facts["workspace"].as_str()?));
    roots.extend(
        facts["scratch_roots"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from),
    );
    let outside = |candidate: &Path| {
        roots.iter().all(|root| {
            !candidate.starts_with(root)
                && !candidate.starts_with(root.canonicalize().unwrap_or_else(|_| root.clone()))
        })
    };
    let resolved = std::env::split_paths(&std::env::var_os("PATH")?)
        .filter_map(|dir| path_directory(dir, facts))
        .map(|dir| dir.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
        })?;
    // Check identity at review time, without skipping an untrusted PATH hit.
    // A root-owned binary replaced between review and spawn is a remaining TOCTOU
    // outside this advisory gate's scope: bash is advisory, not a sandbox (CONTEXT.md).
    let metadata = resolved.metadata().ok()?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return None;
    }
    let canonical = resolved.canonicalize().ok()?;
    // Executable identity, 2026-10-10: mise links point to root-owned binaries;
    // accept secure owned link directories, never user-owned regular executables.
    // The link's name is what the host classifies, so it must not impersonate another
    // program (`~/.local/bin/ls -> /usr/bin/rm`): only same-name links or version-manager
    // dispatchers, which run the tool named by the link, qualify.
    let safe_shim = resolved.symlink_metadata().ok()?.is_symlink()
        && trusted_shim_directories(&resolved)
        && canonical
            .file_name()
            .and_then(|target| target.to_str())
            .is_some_and(|target| target == name || matches!(target, "mise" | "asdf" | "rtx"));
    if (!trusted_directories(&resolved) && !safe_shim)
        || !trusted_directories(&canonical)
        || !outside(&resolved)
        || !outside(&canonical)
        || (path.is_absolute() && (path != canonical || !outside(path)))
    {
        return None;
    }
    // Publication probes deliberately use this exact system binary.
    if name == "git" && canonical != Path::new("/usr/bin/git").canonicalize().ok()? {
        return None;
    }
    Some((name.to_owned(), canonical))
}

fn trusted_directories(path: &Path) -> bool {
    path.ancestors().skip(1).all(|parent| {
        // Check the spelling's chain (including a symlink's containing directory)
        // and every resolved directory's chain, not just the executable target.
        parent.canonicalize().is_ok_and(|canonical| {
            canonical.ancestors().all(|directory| {
                directory.metadata().is_ok_and(|metadata| {
                    metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0
                })
            })
        })
    })
}

fn trusted_shim_directories(path: &Path) -> bool {
    let uid = nix::unistd::getuid().as_raw();
    path.ancestors().skip(1).all(|parent| {
        parent.canonicalize().is_ok_and(|canonical| {
            canonical.ancestors().all(|directory| {
                directory.metadata().is_ok_and(|metadata| {
                    metadata.is_dir()
                        && (metadata.uid() == 0 || metadata.uid() == uid)
                        && metadata.mode() & 0o022 == 0
                })
            })
        })
    })
}

pub(super) fn segments(command: &str) -> Option<Vec<Vec<String>>> {
    let mut segments = vec![];
    let mut words = vec![];
    let mut word = String::new();
    let mut word_started = false;
    let mut quote = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if q == '"' && c == '\\' {
                let next = chars.next()?;
                if !"|.()[]{}+?*".contains(next) {
                    return None;
                }
                word.push('\\');
                word.push(next);
                continue;
            }
            if q == '"' && matches!(c, '$' | '`') {
                return None;
            }
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c.is_whitespace() && !matches!(c, ' ' | '\t' | '\n') {
            // Bash does not split words at CR, VT, FF or Unicode whitespace.
            // Never invent a boundary that could turn an operand's # into a comment.
            return None;
        } else if c == '\'' || c == '"' {
            word_started = true;
            quote = Some(c);
        } else if c == '#' && !word_started {
            // Only inert, single-line comments qualify for literal review. Keep
            // substitutions and continuations opaque even in comment text.
            while chars.peek().is_some_and(|c| *c != '\n') {
                if matches!(chars.next()?, '$' | '`' | '\\' | '(' | ')' | '{' | '}') {
                    return None;
                }
            }
        // Filename globs (`*`, `?`, `[...]`) only pick existing paths: the program and
        // its flags still decide the effect, and sensitive-target checks strip glob
        // characters before matching. Real session, 2026-10-10: `grep … tests/*` and
        // `--include=*.ts` reads were sent to review only because of the glob.
        } else if matches!(c, '$' | '`' | '\\' | '(' | ')' | '{' | '}') {
            return None;
        } else if c == '>' || (c == '&' && chars.peek() == Some(&'>')) {
            word_started = false;
            if c == '&' {
                chars.next();
            }
            if word.chars().all(|c| c.is_ascii_digit()) {
                word.clear();
            }
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            if chars.peek() == Some(&'>') {
                chars.next();
            }
            let fd = chars.peek() == Some(&'&');
            if fd {
                chars.next();
            }
            while chars.peek().is_some_and(|c| matches!(c, ' ' | '\t')) {
                chars.next();
            }
            let mut target = String::new();
            let mut target_quote = None;
            while let Some(&next) = chars.peek() {
                if let Some(q) = target_quote {
                    chars.next();
                    if q == '"' && matches!(next, '$' | '`' | '\\') {
                        return None;
                    }
                    if next == q {
                        target_quote = None;
                    } else {
                        target.push(next);
                    }
                } else if matches!(next, '\'' | '"') {
                    target_quote = Some(next);
                    chars.next();
                } else if next.is_whitespace() && !matches!(next, ' ' | '\t' | '\n') {
                    return None;
                } else if matches!(next, ' ' | '\t' | '\n') || ";|&<>".contains(next) {
                    break;
                } else if matches!(next, '$' | '`' | '\\' | '(' | ')' | '{' | '}' | '#') {
                    return None;
                } else {
                    target.push(next);
                    chars.next();
                }
            }
            if target.is_empty() || target_quote.is_some() {
                return None;
            }
            if !(target == "/dev/null" || (fd && target.chars().all(|c| c.is_ascii_digit()))) {
                words.push(">".into());
                words.push(target);
            }
        } else if matches!(c, ' ' | '\t' | '\n') || ";|&<".contains(c) {
            word_started = false;
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            if ";|&\n".contains(c) && !words.is_empty() {
                segments.push(std::mem::take(&mut words));
            } else if c == '>' || c == '<' {
                words.push(c.to_string());
            }
        } else {
            word_started = true;
            word.push(c);
        }
    }
    if quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    if !words.is_empty() {
        segments.push(words);
    }
    (!segments.is_empty()).then_some(segments)
}

/// Conservative fragments remain evidence even for unsupported shell syntax.
/// They only tighten checks; they are not parsed shell words or git options.
pub(super) fn operand_fragments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(|c: char| c.is_whitespace() || ";|&()'\"=<>".contains(c))
        .filter(|word| !word.is_empty())
}

/// Inspect whole literal paths, including assignment/option values, without
/// weakening the existing fragment checks when parsing fails or succeeds.
pub(super) fn path_operands(command: &str) -> Vec<String> {
    let mut operands: Vec<String> = operand_fragments(command).map(str::to_owned).collect();
    if let Some(parts) = segments(command) {
        for word in parts.into_iter().flatten() {
            if let Some((_, value)) = word.split_once('=') {
                operands.push(value.to_owned());
            }
            operands.push(word);
        }
    }
    operands
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Scheduling wrappers expose their operands; execution-string wrappers do not.
pub(super) fn inner_command(mut words: &[String]) -> Option<&[String]> {
    loop {
        let program = basename(words.first()?);
        let mut i = 1;
        match program {
            "timeout" => {
                while words.get(i).is_some_and(|w| w.starts_with('-')) {
                    let option = words[i].as_str();
                    match option {
                        "--" => {
                            i += 1;
                            break;
                        }
                        "-s" | "--signal" | "-k" | "--kill-after" => i += 2,
                        "--foreground" | "--preserve-status" | "-v" | "--verbose" => i += 1,
                        w if w.starts_with("--signal=") || w.starts_with("--kill-after=") => i += 1,
                        _ => return None,
                    }
                }
                let duration = words.get(i)?;
                if !duration
                    .chars()
                    .all(|c| c.is_ascii_digit() || ".smhd".contains(c))
                    || !duration.chars().any(|c| c.is_ascii_digit())
                {
                    return None;
                }
                i += 1;
            }
            "nice" => {
                match words.get(i).map(String::as_str) {
                    Some("-n" | "--adjustment") => {
                        words.get(i + 1)?.parse::<i64>().ok()?;
                        i += 2;
                    }
                    Some(w) if w.starts_with("--adjustment=") => {
                        w.strip_prefix("--adjustment=")?.parse::<i64>().ok()?;
                        i += 1;
                    }
                    Some(w) if w.starts_with('-') && w != "--" => {
                        w[1..].parse::<i64>().ok()?;
                        i += 1;
                    }
                    _ => {}
                }
                if words.get(i).is_some_and(|w| w == "--") {
                    i += 1;
                }
            }
            "nohup" => {
                if words.get(i).is_some_and(|w| w == "--") {
                    i += 1;
                }
            }
            "env" => {
                while words
                    .get(i)
                    .is_some_and(|w| !w.starts_with('-') && w.contains('='))
                {
                    i += 1;
                }
                if words.get(i)?.starts_with('-') {
                    return None;
                }
            }
            _ => return Some(words),
        }
        words = words.get(i..)?;
    }
}

fn lookup(words: &[String]) -> bool {
    matches!(basename(&words[0]), "type" | "which")
        || (basename(&words[0]) == "command"
            && words
                .get(1)
                .is_some_and(|w| matches!(w.as_str(), "-v" | "-V")))
}

pub(super) fn identity_floor(words: &[String], facts: &Value) -> bool {
    // env wrappers expose their program but must not erase execution-affecting
    // Git assignments from the command being reviewed.
    if words
        .iter()
        .any(|word| word.starts_with("GIT_") && word.contains('='))
    {
        return true;
    }
    let Some(inner) = inner_command(words) else {
        return true;
    };
    words
        .iter()
        .take(words.len() - inner.len())
        .filter(|word| matches!(basename(word), "timeout" | "nice" | "nohup" | "env"))
        .chain(inner.first())
        .any(|word| {
            if lookup(inner) && word == &inner[0] && matches!(word.as_str(), "command" | "type") {
                return false;
            }
            let exists = word.contains('/')
                || std::env::var_os("PATH").is_some_and(|path| {
                    std::env::split_paths(&path).any(|dir| {
                        // Preserve the daemon-cwd check as well as the workspace
                        // check: an additional location must never remove a floor.
                        dir.join(word).metadata().is_ok_and(|m| m.is_file())
                            || path_directory(dir, facts).is_some_and(|dir| {
                                dir.join(word).metadata().is_ok_and(|m| m.is_file())
                            })
                    })
                });
            exists && trusted_program(word, facts).is_none()
        })
}

/// Only classified directory-neutral commands may preserve relative PATH lookup.
/// Shell prefixes run builtins in this shell; execution strings remain opaque.
fn changes_directory(words: &[String], facts: &Value) -> Option<bool> {
    let mut words = inner_command(words)?;
    loop {
        match words.first()?.as_str() {
            "builtin" | "command" => {
                let command = words[0] == "command";
                let mut i = 1;
                while let Some(option) = words.get(i).filter(|word| word.starts_with('-')) {
                    match option.as_str() {
                        "--" => {
                            i += 1;
                            break;
                        }
                        "-v" | "-V" if command => return Some(false),
                        "-p" if command => i += 1,
                        _ => return None,
                    }
                }
                words = words.get(i..)?;
            }
            "cd" | "pushd" | "popd" => return Some(true),
            "echo" | "printf" | "pwd" | "true" | "false" | ":" | "type" => return Some(false),
            "exec" | "eval" | "source" | "." => return None,
            program => return trusted_program(program, facts).map(|_| false),
        }
    }
}

pub(super) fn command_identity_floor(command: &str, facts: &Value) -> bool {
    segments(command).is_none_or(|parts| {
        let relative_path = std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| !dir.is_absolute()));
        let mut changed_directory = false;
        parts.iter().any(|words| {
            // Later PATH lookup is no longer relative to the reviewed workspace.
            // Only literal shell builtins and absolute programs avoid that ambiguity.
            if changed_directory
                && relative_path
                && inner_command(words).is_none_or(|inner| {
                    !Path::new(&inner[0]).is_absolute()
                        && (inner.len() != words.len()
                            || !matches!(
                                inner[0].as_str(),
                                "echo"
                                    | "printf"
                                    | "pwd"
                                    | "true"
                                    | "false"
                                    | ":"
                                    | "cd"
                                    | "pushd"
                                    | "popd"
                                    | "command"
                                    | "type"
                            ))
                })
            {
                return true;
            }
            changed_directory |= changes_directory(words, facts).unwrap_or(true);
            identity_floor(words, facts)
        })
    })
}

pub(super) fn execution_floor(command: &str, facts: &Value) -> bool {
    command_identity_floor(command, facts) || segments(command).is_none_or(|parts| {
        parts.iter().any(|words| {
            inner_command(words).is_none_or(|inner| {
                let program = basename(&inner[0]);
             identity_floor(words, facts)
                 || destructive(inner)
                 || git_read_side_effect(inner)
                // Shell identity, 2026-10-10: aliases can hide shell bodies;
                // keep that floor while leaving ordinary programs to Instinct.
                || (program == "git" && git_subcommand(inner).is_none_or(|i| !matches!(inner[i].as_str(), "add" | "commit" | "status" | "diff" | "log" | "show" | "push" | "restore" | "reset" | "checkout" | "switch" | "branch" | "remote" | "ls-files" | "rev-parse" | "fetch" | "merge" | "rebase" | "stash" | "tag" | "clean" | "config")))
                    || (program == "git"
                        && git_subcommand(inner).is_some_and(|i| inner[i] == "push")
                        && !understood(inner, facts, command))
                    || inner[0].starts_with("./")
                    || inner[0].contains('=')
                    || matches!(
                        program,
                        "sh" | "bash" | "zsh" | "eval" | "source" | "." | "sudo" | "xargs" | "exec"
                    )
                    || (matches!(program, "command" | "builtin") && !lookup(inner))
                    || (matches!(program, "python" | "python3" | "node" | "perl" | "ruby")
                        && (inner.get(1).is_some_and(|w| !w.starts_with('-'))
                            || inner.iter().any(|w| matches!(w.as_str(), "-c" | "-e"))))
            })
        })
    })
}

// Wrappers/options are not interpreted optimistically. Still locate the real executable
// for destructive/publication classification (including git -C and absolute paths).
fn executable(words: &[String]) -> Option<usize> {
    words.iter().position(|word| {
        !word.contains('=')
            && !matches!(
                basename(word),
                "env" | "command" | "exec" | "nice" | "sudo" | "nohup" | "timeout"
            )
            && !word.starts_with('-')
            && word.parse::<i64>().is_err()
    })
}

fn git_subcommand(words: &[String]) -> Option<usize> {
    let mut i = 1;
    while i < words.len() {
        match words[i].as_str() {
            "-C" | "-c" | "--config-env" | "--git-dir" | "--work-tree" | "--namespace"
            | "--super-prefix" => {
                words.get(i + 1)?;
                i += 2;
            }
            "-p"
            | "--paginate"
            | "-P"
            | "--no-pager"
            | "--bare"
            | "--no-replace-objects"
            | "--no-optional-locks"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-lazy-fetch"
            | "--no-advice" => {
                i += 1;
            }
            w if w.starts_with("-C")
                || w.starts_with("-c") && !w.starts_with("--")
                || [
                    "--config-env=",
                    "--git-dir=",
                    "--work-tree=",
                    "--namespace=",
                    "--super-prefix=",
                    "--exec-path=",
                ]
                .iter()
                .any(|prefix| w.starts_with(prefix)) =>
            {
                i += 1;
            }
            // --exec-path without '=' and --list-cmds= print and exit. Unknown
            // options may consume a value: never guess a following subcommand.
            w if w.starts_with('-') => return None,
            _ => return Some(i),
        }
    }
    None
}

fn git_read_side_effect(words: &[String]) -> bool {
    basename(&words[0]) == "git"
        && git_subcommand(words).is_none_or(|i| {
            // Configuration, cwd, repository and executable overrides can run
            // helpers. Only explicitly inert global options preserve read safety.
            words[1..i]
                .iter()
                .any(|word| !matches!(word.as_str(), "--no-pager" | "--no-optional-locks"))
                || (words[i] == "diff" && !safe_diff_options(&words[i + 1..]))
                || matches!(words[i].as_str(), "status" | "diff" | "log" | "show")
                    && words[i + 1..].iter().any(|word| {
                        matches!(
                            word.split('=').next(),
                            Some("--exec" | "--ext-diff" | "--textconv")
                        ) || word.starts_with("--output")
                    })
        })
}

fn safe_diff_options(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            continue;
        }
        if matches!(
            arg.as_str(),
            "-p" | "--patch"
                | "-s"
                | "--no-patch"
                | "-u"
                | "--stat"
                | "--numstat"
                | "--shortstat"
                | "--summary"
                | "--name-only"
                | "--name-status"
                | "--check"
                | "--exit-code"
                | "--quiet"
                | "--cached"
                | "--staged"
                | "--no-ext-diff"
                | "--no-textconv"
                | "--no-color"
                | "--color=never"
                | "--color=always"
                | "--color=auto"
                | "-w"
                | "--ignore-all-space"
                | "-b"
                | "--ignore-space-change"
                | "--ignore-space-at-eol"
                | "--ignore-cr-at-eol"
                | "--ignore-blank-lines"
                | "--binary"
                | "--full-index"
                | "--no-renames"
                | "--no-index"
        ) {
            continue;
        }
        let context = arg
            .strip_prefix("-U")
            .or_else(|| arg.strip_prefix("--unified="));
        if !context
            .is_some_and(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        {
            return false;
        }
    }
    true
}

pub(super) fn destructive(words: &[String]) -> bool {
    let Some(words) = inner_command(words) else {
        return true;
    };
    if lookup(words) {
        return false;
    }
    let Some(i) = executable(words) else {
        return true;
    };
    let words = &words[i..];
    let program = basename(&words[0]);
    let has = |s: &str| words.iter().any(|w| w == s);
    match program {
        "git" => {
            let Some(i) = git_subcommand(words) else {
                return true;
            };
            let args = &words[i + 1..];
            let opt = |s: &str| args.iter().any(|w| w == s);
            match words[i].as_str() {
                "push" => args
                    .iter()
                    .any(|word| super::git::captain_only_push_operand(word)),
                "checkout" => opt("--") || opt("."),
                "restore" => !opt("--staged") || opt("--worktree") || opt("-W"),
                "stash" => args.iter().any(|w| matches!(w.as_str(), "drop" | "clear")),
                "reset" => opt("--hard"),
                "branch" => {
                    opt("-D")
                        || ((opt("--delete") || opt("-d")) && (opt("--force") || opt("-f")))
                        || args.iter().any(|w| {
                            w.starts_with('-')
                                && !w.starts_with("--")
                                && w.contains('d')
                                && w.contains('f')
                        })
                }
                "clean" => args.iter().any(|w| {
                    w == "--force"
                        || (w.starts_with('-') && !w.starts_with("--") && w.contains('f'))
                }),
                _ => false,
            }
        }
        "gh" => matches!(
            (
                words.get(1).map(String::as_str),
                words.get(2).map(String::as_str)
            ),
            (Some("pr"), Some("merge"))
                | (Some("repo"), Some("delete"))
                | (Some("release"), Some("create" | "delete"))
        ),
        "npm" | "cargo" => has("publish"),
        "twine" => has("upload"),
        "rm" => words
            .iter()
            .any(|w| w == "--recursive" || (w.starts_with('-') && w.contains('r'))),
        "find" => has("-delete") || has("-exec") || has("-execdir"),
        "dd" | "truncate" | "mkfs" => true,
        p if p.starts_with("mkfs.") => true,
        _ => false,
    }
}

/// Unknown push options may be destructive clusters or Git's long abbreviations.
/// Keep the Captain hold even when metacognition approves, including compound calls.
pub(super) fn captain_only_push(command: &str) -> bool {
    let push_uncertainty = || {
        // Normalisation cannot decode expansions: an opaque Git subcommand may
        // become push. Non-Git expansions retain ordinary metacognition review.
        let normalised = command.replace(['\\', '\'', '"'], "");
        normalised.contains("push")
            || (normalised
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|word| word == "git")
                && (command.contains(['$', '`'])
                    || command.contains("<(")
                    || command.contains(">(")))
    };
    let Some(parts) = segments(command) else {
        return push_uncertainty();
    };
    parts.iter().any(|words| {
        let Some(words) = inner_command(words) else {
            return push_uncertainty();
        };
        let Some(i) = executable(words) else {
            return push_uncertainty();
        };
        let words = &words[i..];
        basename(&words[0]) == "git"
            && git_subcommand(words).map_or_else(
                || words[1..].iter().any(|word| word == "push"),
                |i| {
                    words[i] == "push"
                        && words[i + 1..]
                            .iter()
                            .any(|word| super::git::captain_only_push_operand(word))
                },
            )
    })
}

pub(super) fn understood(words: &[String], facts: &Value, whole: &str) -> bool {
    let Some(i) = executable(words) else {
        return false;
    };
    if i != 0 || words.iter().any(|w| w.contains('=')) {
        return false;
    }
    let Some((program, _)) = trusted_program(&words[0], facts) else {
        return false;
    };
    if words.iter().any(|w| w == "<") {
        return false;
    }
    let args = &words[1..];
    match program.as_str() {
        "git" => {
            let Some(i) = git_subcommand(words) else {
                return false;
            };
            if i != 1 {
                return false;
            }
            match words[i].as_str() {
                "push" => super::git::literal(whole)
                    .and_then(|w| super::git::push(&w, facts["git"]["current_branch"].as_str()))
                    .is_some_and(|push| {
                        facts["git"]["non_default_push"] == true
                            && facts["git"]["safe_push_config"] == true
                            && push["literal_target"] == true
                            && push["own_branch"] == true
                            && push["force"] == false
                            && push["delete"] == false
                            && facts["git"]["default_branch"]
                                .as_str()
                                .is_some_and(|default| {
                                    push["branch"]
                                        .as_str()
                                        .is_some_and(|branch| branch != default)
                                })
                    }),
                "add" => words[2..].iter().all(|w| {
                    !w.starts_with('-')
                        || matches!(w.as_str(), "--" | "-A" | "--all" | "-u" | "--update")
                }),
                "commit" => {
                    let mut args = words[2..].iter();
                    let mut message = false;
                    let mut safe = true;
                    while let Some(arg) = args.next() {
                        match arg.as_str() {
                            "-a" | "--all" => {}
                            "-m" | "-am" | "--message" => {
                                message = args.next().is_some();
                                safe &= message;
                            }
                            _ => safe = false,
                        }
                    }
                    safe && message
                }
                "status" | "diff" | "log" | "show" => !git_read_side_effect(words),
                _ => false,
            }
        }
        "sh" | "bash" | "zsh" | "eval" | "source" | "." | "xargs" => false,
        "python3" => args == ["-I", "-S", "-m", "json.tool"],
        "gh" => matches!(
            (
                args.first().map(String::as_str),
                args.get(1).map(String::as_str)
            ),
            (Some("pr"), Some("view" | "list" | "checks" | "edit"))
        ),
        "cargo" => matches!(
            args.first().map(String::as_str),
            Some("test" | "build" | "clippy" | "fmt" | "check")
        ),
        "npm" => matches!(args.first().map(String::as_str), Some("ci" | "test")),
        "make" => args == ["test"],
        "cp" | "mv" => args.len() == 2 && args.iter().all(|w| !w.starts_with('-')),
        "sed" => sed_paths(args).is_some(),
        "perl" => false,
        "rm" | "unlink" | "shred" => {
            !args.is_empty()
                && args
                    .iter()
                    .all(|w| !w.starts_with('-') || matches!(w.as_str(), "--" | "-f" | "--force"))
        }
        "cat" | "head" | "tail" | "wc" | "ls" | "stat" | "grep" | "diff" | "file" | "find"
        | "echo" | "printf" | "mkdir" | "touch" | "jq" => true,
        _ => false,
    }
}

/// Only literal interpreter operands or explicit executable paths are evidence candidates.
pub(super) fn script_operand<'a>(words: &'a [String], facts: &Value) -> Option<(&'a str, bool)> {
    let first = words.first()?;
    if words
        .iter()
        .any(|word| word.contains('=') || matches!(word.as_str(), "<" | ">"))
    {
        return None;
    }
    let trusted = trusted_program(first, facts);
    let index = match trusted.as_ref().map(|(name, _)| name.as_str()) {
        Some("python" | "python3" | "bash" | "sh" | "node") => 1,
        Some("deno" | "uv") if words.get(1).is_some_and(|word| word == "run") => 2,
        _ if first.starts_with("./") || first.starts_with('/') || first.starts_with("~/") => {
            return Some((first, true));
        }
        _ => return None,
    };
    let operand = words.get(index)?;
    (!operand.starts_with('-')).then_some((operand, false))
}

pub(super) fn requires_review(command: &str, facts: &Value) -> bool {
    if command.contains('|') {
        return !read_only_pipeline(command, facts);
    }
    let Some(segments) = segments(command) else {
        return true;
    };
    segments
        .iter()
        .any(|words| destructive(words) || !understood(words, facts, command))
        || (command.contains('|') && segments.iter().any(|words| !read_only(words)))
}

/// Literal read classifications may bypass the small mutation vocabulary, but
/// command-hiding wrappers and deterministic writes still need review.
pub(super) fn read_floor(command: &str) -> bool {
    let Some(parts) = segments(command) else {
        return true;
    };
    removal_or_move(command)
        || overwrite_paths(command).is_none_or(|paths| !paths.is_empty())
        || parts.iter().any(|words| {
            let Some(words) = inner_command(words) else {
                return true;
            };
            if lookup(words) {
                return false;
            }
            let program = basename(&words[0]);
            destructive(words)
                || git_read_side_effect(words)
                || words[0].starts_with("./")
                // These write or change files through their operands rather than
                // a `>` redirection, so the redirection parser cannot see them.
                || matches!(
                    program,
                    "cp" | "mv"
                        | "rm"
                        | "unlink"
                        | "shred"
                        | "truncate"
                        | "tee"
                        | "ln"
                        | "install"
                        | "chmod"
                        | "chown"
                        | "chgrp"
                )
                || (program == "git"
                    && git_subcommand(words).is_none_or(|i| {
                        !matches!(
                            words[i].as_str(),
                            "status"
                                | "diff"
                                | "log"
                                | "show"
                                | "ls-files"
                                | "rev-parse"
                                | "remote"
                                | "branch"
                        )
                    }))
                || (matches!(program, "python" | "python3" | "node" | "perl" | "ruby")
                    && words.get(1).is_some_and(|w| !w.starts_with('-')))
                || (program == "perl" && words.iter().any(|w| w.starts_with("-i")))
                || words[0].contains('=')
                || matches!(
                    program,
                    "sh" | "bash"
                        | "zsh"
                        | "eval"
                        | "source"
                        | "."
                        | "xargs"
                        | "sudo"
                        | "exec"
                         | "command"
                         | "builtin"
                )
                || (matches!(program, "python" | "python3" | "node" | "perl" | "ruby")
                    && words.iter().any(|w| matches!(w.as_str(), "-c" | "-e")))
        })
}

/// Exact stdin-only transforms; no file loading, modules or arbitrary Python.
pub(super) fn read_only_pipeline(command: &str, facts: &Value) -> bool {
    let audited = "python3 -I -S -c 'import json, sys; print(json.dumps(json.load(sys.stdin)))'";
    let checked = command.replace(audited, "python3 -I -S -m json.tool");
    if segments(&checked).is_none() || checked.contains("||") {
        return false;
    }
    let stages: Vec<_> = command.split('|').map(str::trim).collect();
    if stages.len() < 2 || stages.iter().any(|s| s.is_empty()) {
        return false;
    }
    let Some(producer) = segments(stages[0]).filter(|s| s.len() == 1) else {
        return false;
    };
    let words = &producer[0];
    if !read_only(words) || !understood(words, facts, stages[0]) || destructive(words) {
        return false;
    }
    let Some((program, _)) = trusted_program(&words[0], facts) else {
        return false;
    };
    if !matches!(program.as_str(), "git" | "gh" | "cat") {
        return false;
    }
    if program == "gh" && !words.iter().any(|w| w == "--json") {
        return false;
    }
    if program == "cat"
        && (words.len() < 2
            || words[1..].iter().any(|w| {
                w.starts_with('-')
                    || !Path::new(facts["workspace"].as_str().unwrap_or(""))
                        .join(w)
                        .canonicalize()
                        .is_ok_and(|p| {
                            p.starts_with(facts["workspace"].as_str().unwrap_or(""))
                                && !super::sensitive_read(&p)
                        })
            }))
    {
        return false;
    }
    stages[1..].iter().all(|stage| {
        // This audited program only parses stdin JSON and writes stdout JSON.
        if *stage == audited {
            return trusted_program("python3", facts).is_some();
        }
        let Some(parts) = segments(stage).filter(|s| s.len() == 1) else {
            return false;
        };
        let words = &parts[0];
        match trusted_program(&words[0], facts)
            .as_ref()
            .map(|(name, _)| name.as_str())
        {
            Some("python3") => words[1..] == ["-I", "-S", "-m", "json.tool"],
            Some("jq") => words.len() == 2 && safe_jq_filter(&words[1]),
            Some("head" | "tail" | "grep" | "wc" | "sort" | "uniq" | "cut" | "cat") => {
                read_only(words)
            }
            _ => false,
        }
    })
}

fn safe_jq_filter(filter: &str) -> bool {
    // A deliberately small filter grammar: identity and property selection.
    filter.starts_with('.')
        && filter
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))
}

fn read_only(words: &[String]) -> bool {
    if words.iter().any(|w| w == ">") {
        return false;
    }
    match basename(&words[0]) {
        "git" => matches!(
            words.get(1).map(String::as_str),
            Some("status" | "diff" | "log" | "show")
        ),
        "gh" => matches!(
            words.get(2).map(String::as_str),
            Some("view" | "list" | "checks")
        ),
        "python3" => words[1..] == ["-I", "-S", "-m", "json.tool"],
        "cat" | "head" | "tail" | "wc" | "ls" | "stat" | "grep" | "diff" | "file" | "find"
        | "echo" | "printf" | "sort" | "uniq" | "cut" => true,
        _ => false,
    }
}

pub(super) fn overwrite_paths(command: &str) -> Option<Vec<String>> {
    let mut paths = vec![];
    for words in segments(command)? {
        let words = inner_command(&words)?;
        for pair in words.windows(2) {
            if pair[0] == ">" && pair[1] != ">" {
                paths.push(pair[1].clone());
            }
        }
        match basename(&words[0]) {
            "cp" => paths.push(words.last()?.clone()),
            "mv" => paths.extend(
                words
                    .iter()
                    .skip(1)
                    .filter(|w| !w.starts_with('-'))
                    .cloned(),
            ),
            "rm" | "unlink" | "shred" | "truncate" => paths.extend(
                words
                    .iter()
                    .skip(1)
                    .filter(|w| !w.starts_with('-'))
                    .cloned(),
            ),
            "sed" if in_place(&words[1..]) => paths.extend(sed_paths(&words[1..])?),
            // Without -i, sed prints to stdout, unless its script writes files (`w`,
            // `s///w`) or runs commands (`e`, `s///e`): only recognised print and
            // substitution scripts are known to change nothing.
            "sed" if !stdout_sed(&words[1..]) => return None,
            _ => {}
        }
    }
    Some(paths)
}

fn in_place(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg.starts_with("-i") || arg.starts_with("--in-place"))
}

fn stdout_sed(args: &[String]) -> bool {
    // An address (line numbers, `$`, or a `/regex/`) followed by at most one p, d or q.
    let print = |script: &str| {
        script.split(';').all(|part| {
            let part = part.trim();
            let address = part.trim_end_matches(['p', 'd', 'q']);
            address.len() + 1 >= part.len()
                && address.split(',').all(|a| {
                    a.chars().all(|c| c.is_ascii_digit() || c == '$')
                        || (a.len() >= 2
                            && a.starts_with('/')
                            && a.ends_with('/')
                            && !a[1..a.len() - 1].contains('/'))
                })
        })
    };
    let substitution = |script: &str| {
        let parts: Vec<_> = script.split('/').collect();
        parts.len() == 4
            && parts[0] == "s"
            && parts[3]
                .chars()
                .all(|c| matches!(c, 'g' | 'i' | 'p' | '0'..='9'))
    };
    // GNU sed accepts options after operands, and with any -e the first operand is a
    // file, not the script: scan every argument before deciding what the script is.
    let mut scripts = vec![];
    let mut operands = vec![];
    let mut options_done = false;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            _ if options_done => operands.push(arg.as_str()),
            "--" => options_done = true,
            "-n" | "--quiet" | "--silent" | "-E" | "-r" | "--regexp-extended" | "-s"
            | "--separate" | "-u" | "--unbuffered" | "-z" | "--null-data" => {}
            "-e" | "--expression" => {
                i += 1;
                match args.get(i) {
                    Some(script) => scripts.push(script.as_str()),
                    None => return false,
                }
            }
            option if option.starts_with('-') && option != "-" => return false,
            operand => operands.push(operand),
        }
        i += 1;
    }
    if scripts.is_empty() {
        match operands.first() {
            Some(script) => scripts.push(script),
            None => return false,
        }
    }
    scripts
        .iter()
        .all(|script| print(script) || substitution(script))
}

fn sed_paths(args: &[String]) -> Option<Vec<String>> {
    if args.first()?.as_str() != "-i" {
        return None;
    }
    let substitution = |expression: &str| {
        expression.split(';').all(|expr| {
            let parts: Vec<_> = expr.split('/').collect();
            parts.len() == 4
                && parts[0] == "s"
                && parts[3]
                    .chars()
                    .all(|c| matches!(c, 'g' | 'i' | 'p' | '0'..='9'))
        })
    };
    let mut i = 1;
    if args.get(i)?.as_str() == "-e" {
        while args.get(i).is_some_and(|arg| arg == "-e") {
            if !substitution(args.get(i + 1)?) {
                return None;
            }
            i += 2;
        }
    } else {
        if !substitution(args.get(i)?) {
            return None;
        }
        i += 1;
    }
    let paths = &args[i..];
    (!paths.is_empty() && paths.iter().all(|p| !p.starts_with('-'))).then(|| paths.to_vec())
}

pub(super) fn removal_or_move(command: &str) -> bool {
    segments(command).is_none_or(|segments| {
        segments.iter().any(|words| {
            executable(words).is_none_or(|i| {
                matches!(
                    basename(&words[i]),
                    "rm" | "unlink" | "shred" | "truncate" | "mv"
                )
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn literal_regex_and_transparent_wrappers_preserve_effects() {
        assert!(segments(r#"grep -n "^name\|^version" Cargo.toml"#).is_some());
        for command in [
            r#"echo "$HOME""#,
            r#"echo "\$x""#,
            "git status # $(evil)",
            "echo a#$(evil)",
            "echo ''#$(evil)",
            "git status # continuation\\\nrm -r x",
        ] {
            assert!(segments(command).is_none());
        }
        for (command, expected) in [
            ("git status # inspection", vec![vec!["git", "status"]]),
            ("echo a#b", vec![vec!["echo", "a#b"]]),
            ("echo ''#b", vec![vec!["echo", "#b"]]),
            ("echo '#'", vec![vec!["echo", "#"]]),
            (
                "git status # inspection\nrm -r x",
                vec![vec!["git", "status"], vec!["rm", "-r", "x"]],
            ),
        ] {
            assert_eq!(segments(command).unwrap(), expected, "{command}");
        }
        for command in [
            "timeout 15 ls",
            "timeout -k 2s 15 ls",
            "nice -n 5 ls",
            "nohup ls",
            "env LANG=C ls",
            "command -v nix",
            "command -V nix",
            "type nix",
            "which nix",
        ] {
            assert!(!read_floor(command), "{command}");
        }
        assert!(destructive(&segments("timeout 15 rm -r x").unwrap()[0]));
        for command in ["env -i ls", "env -S ls", "timeout 15 bash -c 'make'"] {
            assert!(read_floor(command));
        }
    }

    #[test]
    fn secure_user_shim_accepts_system_target_but_not_regular_binary() {
        const CHILD: &str = "WITHNAV_TEST_SECURE_SHIM";
        if let Ok(workspace) = std::env::var(CHILD) {
            let facts = json!({"workspace":workspace,"writable_roots":[workspace]});
            assert!(trusted_program("cargo", &facts).is_some());
            assert!(trusted_program("local-binary", &facts).is_none());
            assert!(trusted_program("ls", &facts).is_none());
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::var_os("HOME").unwrap();
        let tmp = tempfile::tempdir_in(home).unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink("/usr/bin/mise", bin.join("cargo")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/rm", bin.join("ls")).unwrap();
        std::fs::copy("/usr/bin/true", bin.join("local-binary")).unwrap();
        std::fs::set_permissions(
            bin.join("local-binary"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agent::permission_gate::shell::tests::secure_user_shim_accepts_system_target_but_not_regular_binary"])
            .env(CHILD, "/nonexistent-workspace")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn printing_sed_and_globs_are_reads_but_writing_sed_is_not() {
        for command in [
            "sed -n 250,285p tests/settings.test.cjs",
            "sed -n '78,90p' src/webview/settingsModel.ts",
            "sed -n '/fn main/,/^}/p' src/main.rs",
            "sed -n -e 1p -e '$p' notes.md",
            "sed 's/a/b/g' notes.md",
            "sed -n 1,5p a.txt -E",
            "grep -n theme tests/*",
            "ls src/*.rs",
        ] {
            assert_eq!(overwrite_paths(command), Some(vec![]), "{command}");
            assert!(!read_floor(command), "{command}");
        }
        for command in [
            "sed -n '1w out.txt' notes.md",
            "sed 's/a/b/w out.txt' notes.md",
            "sed 's/a/b/e' notes.md",
            "sed -n 1e notes.md",
            "sed -f script.sed notes.md",
            "sed -e 'w out.txt' notes.md",
            "sed -i 's/a/b/' notes.md",
            "sed --in-place 's/a/b/' notes.md",
            "rm *.log",
            "cp * backup",
        ] {
            assert!(read_floor(command), "{command}");
        }
    }

    #[test]
    fn fd_redirections_do_not_create_writes_or_command_boundaries() {
        for command in [
            "ls x 2>&1",
            "ls x >&2",
            "ls x 1>&2",
            "cmd 2>/dev/null",
            "ls >/dev/null",
            "ls &>/dev/null",
        ] {
            let parts = segments(command).unwrap();
            assert_eq!(parts.len(), 1, "{command}");
            assert!(!parts[0].iter().any(|w| w == ">"), "{command}");
            assert_eq!(overwrite_paths(command), Some(vec![]), "{command}");
            assert!(!read_floor(command), "{command}");
        }
        for (command, target) in [
            ("ls > out.txt", "out.txt"),
            ("echo x 2> err.log", "err.log"),
            ("echo x >> 'out.txt'", "out.txt"),
        ] {
            assert_eq!(overwrite_paths(command), Some(vec![target.into()]));
            assert!(!read_only(&segments(command).unwrap()[0]));
        }
        for command in ["ls | tee out.txt", "chmod +x run.sh", "ln -s a b"] {
            assert!(read_floor(command), "{command}");
        }
        for command in [
            r#"echo x > "$HOME/out""#,
            r#"echo x > "$(evil)""#,
            r#"echo x > "`evil`""#,
            r#"echo x > "\"; evil; echo \"""#,
            "echo x > $HOME/out",
            "echo x > $(evil)",
            "echo x > out\\ file",
        ] {
            assert!(segments(command).is_none(), "{command}");
            assert!(read_floor(command), "{command}");
        }
        assert_eq!(
            overwrite_paths("echo x > '$HOME/out'"),
            Some(vec!["$HOME/out".into()])
        );
    }

    #[test]
    fn script_and_command_identity_reject_system_symlinks_in_writable_path_directory() {
        const CHILD: &str = "WITHNAV_TEST_WRITABLE_PATH_SYMLINK";
        if let Ok(workspace) = std::env::var(CHILD) {
            let facts = json!({"workspace":workspace,"writable_roots":[workspace]});
            // The first executable PATH hit is outside registered roots and points
            // at a root-owned system binary. Its containing directory is untrusted.
            assert!(trusted_program("python3", &facts).is_none());
            assert!(script_operand(&["python3".into(), "local.py".into()], &facts).is_none());
            assert!(trusted_program("git", &facts).is_none());
            assert!(identity_floor(&["workspace-only".into()], &facts));
            let call = crate::provider::ToolCall {
                id: "command".into(),
                name: "bash".into(),
                args: json!({"command":"git status"}),
                args_valid: true,
            };
            assert!(super::super::context::signature(&call, &facts).is_none());
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let bin = tmp.path().join("unregistered-bin");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let relative_bin = workspace.join("bin");
        std::fs::create_dir(&relative_bin).unwrap();
        std::os::unix::fs::symlink("/usr/bin/true", relative_bin.join("workspace-only")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o777)).unwrap();
        for name in ["python3", "git"] {
            std::os::unix::fs::symlink(format!("/usr/bin/{name}"), bin.join(name)).unwrap();
        }
        // Isolate PATH changes from other parallel tests.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent::permission_gate::shell::tests::script_and_command_identity_reject_system_symlinks_in_writable_path_directory",
                "--nocapture",
            ])
            .env(CHILD, &workspace)
            .env("PATH", format!("bin:{}:/usr/bin:/bin", bin.display()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
