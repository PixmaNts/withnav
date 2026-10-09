//! Conservative shell understanding: unknown execution can never inherit optimistic labels.
use serde_json::Value;

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
    let program = basename(&words[0]);
    if words.iter().any(|w| w == "<") {
        return false;
    }
    let args = &words[1..];
    match program {
        "git" => {
            let Some(i) = git_subcommand(words) else {
                return false;
            };
            if i != 1 {
                return false;
            }
            match words[i].as_str() {
                "push" => super::git::literal(whole)
                    .and_then(|w| super::git::push(&w, facts["current_branch"].as_str()))
                    .is_some_and(|push| {
                        facts["non_default_push"] == true
                            && facts["safe_push_config"] == true
                            && push["literal_target"] == true
                            && push["own_branch"] == true
                            && push["force"] == false
                            && push["delete"] == false
                            && facts["default_branch"].as_str().is_some_and(|default| {
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
        "sed" => {
            args.len() == 3
                && args[0] == "-i"
                && !args[2].starts_with('-')
                && args[1].starts_with("s/")
                && args[1].ends_with('/')
        }
        "cat" | "head" | "tail" | "wc" | "ls" | "stat" | "grep" | "diff" | "file" | "find"
        | "echo" | "printf" | "mkdir" | "touch" | "rm" => true,
        _ => false,
    }
}

pub(super) fn requires_review(command: &str, facts: &Value) -> bool {
    let Some(segments) = segments(command) else {
        return true;
    };
    segments
        .iter()
        .any(|words| destructive(words) || !understood(words, facts, command))
        || (command.contains('|') && segments.iter().any(|words| !read_only(words)))
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
            "cp" | "mv" => paths.push(words.last()?.clone()),
            "sed" if words.iter().any(|w| w.starts_with("-i")) => paths.push(words.last()?.clone()),
            _ => {}
        }
    }
    Some(paths)
}
