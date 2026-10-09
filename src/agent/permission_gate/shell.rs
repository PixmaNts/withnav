//! Conservative shell understanding: unknown execution can never inherit optimistic labels.
use serde_json::Value;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Resolve the actual PATH executable, not a script borrowing a tool's basename.
/// Both its spelling and symlink target must be outside every writable root.
pub(super) fn trusted_program(word: &str, facts: &Value) -> Option<String> {
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
        .map(|dir| {
            if dir.is_absolute() {
                dir
            } else {
                Path::new(facts["workspace"].as_str().unwrap()).join(dir)
            }
        })
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
    if !outside(&resolved)
        || !outside(&canonical)
        || (path.is_absolute() && (path != canonical || !outside(path)))
    {
        return None;
    }
    // Publication probes deliberately use this exact system binary.
    if name == "git" && canonical != Path::new("/usr/bin/git").canonicalize().ok()? {
        return None;
    }
    Some(name.to_owned())
}

pub(super) fn segments(command: &str) -> Option<Vec<Vec<String>>> {
    if command.contains(['$', '`', '\\', '(', ')', '{', '}', '*', '?', '[', '#']) {
        return None;
    }
    let mut segments = vec![];
    let mut words = vec![];
    let mut word = String::new();
    let mut quote = None;
    for c in command.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c.is_whitespace() || ";|&<>".contains(c) {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            if ";|&\n".contains(c) && !words.is_empty() {
                segments.push(std::mem::take(&mut words));
            } else if c == '>' || c == '<' {
                words.push(c.to_string());
            }
        } else {
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

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
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
            "-C" | "-c" | "--git-dir" | "--work-tree" => i += 2,
            w if w.starts_with('-') => i += 1,
            _ => return Some(i),
        }
    }
    None
}

pub(super) fn destructive(words: &[String]) -> bool {
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
                "push" => args.iter().any(|w| {
                    matches!(
                        w.as_str(),
                        "-f" | "--force"
                            | "--force-with-lease"
                            | "-d"
                            | "--delete"
                            | "--all"
                            | "--mirror"
                            | "--tags"
                            | "--follow-tags"
                    ) || w.starts_with("--force-with-lease=")
                        || w.starts_with('+')
                        || w.starts_with(':')
                }),
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

pub(super) fn understood(words: &[String], facts: &Value, whole: &str) -> bool {
    let Some(i) = executable(words) else {
        return false;
    };
    if i != 0 || words.iter().any(|w| w.contains('=')) {
        return false;
    }
    let Some(program) = trusted_program(&words[0], facts) else {
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
                "status" | "diff" | "log" | "show" => !args.iter().any(|w| {
                    matches!(w.as_str(), "--exec" | "--ext-diff" | "--textconv")
                        || w.starts_with("--output")
                }),
                _ => false,
            }
        }
        "sh" | "bash" | "zsh" | "eval" | "source" | "." | "xargs" => false,
        "python3" => args == ["-m", "json.tool"],
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

/// Exact stdin-only transforms; no file loading, modules or arbitrary Python.
pub(super) fn read_only_pipeline(command: &str, facts: &Value) -> bool {
    let audited = "python3 -c 'import json, sys; print(json.dumps(json.load(sys.stdin)))'";
    let checked = command.replace(audited, "python3 -m json.tool");
    if checked.contains(['$', '`', '\\', ';', '&', '<', '>', '\n']) || checked.contains("||") {
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
    let Some(program) = trusted_program(&words[0], facts) else {
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
        if *stage == "python3 -c 'import json, sys; print(json.dumps(json.load(sys.stdin)))'" {
            return trusted_program("python3", facts).is_some();
        }
        let Some(parts) = segments(stage).filter(|s| s.len() == 1) else {
            return false;
        };
        let words = &parts[0];
        match trusted_program(&words[0], facts).as_deref() {
            Some("python3") => words[1..] == ["-m", "json.tool"],
            Some("jq") => words.len() == 2 && safe_jq_filter(&words[1]),
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
    match basename(&words[0]) {
        "git" => matches!(
            words.get(1).map(String::as_str),
            Some("status" | "diff" | "log" | "show")
        ),
        "gh" => matches!(
            words.get(2).map(String::as_str),
            Some("view" | "list" | "checks")
        ),
        "python3" => words[1..] == ["-m", "json.tool"],
        "cat" | "head" | "tail" | "wc" | "ls" | "stat" | "grep" | "diff" | "file" | "find"
        | "echo" | "printf" => !words.iter().any(|w| w == ">"),
        _ => false,
    }
}

pub(super) fn overwrite_paths(command: &str) -> Option<Vec<String>> {
    let mut paths = vec![];
    for words in segments(command)? {
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
            "sed" => paths.extend(sed_paths(&words[1..])?),
            _ => {}
        }
    }
    Some(paths)
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
