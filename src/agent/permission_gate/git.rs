//! Only simple, explicitly read-only git invocations escape the mutation override.
pub(super) fn forces_mutation(command: &str) -> bool {
    if command.chars().any(|c| ";|&><`$()'\"\\\n".contains(c)) {
        return true;
    }
    let words: Vec<_> = command.split_whitespace().collect();
    if words.first().and_then(|word| word.rsplit('/').next()) != Some("git") {
        return true;
    }
    if words.iter().any(|word| {
        [
            "-c",
            "--config-env",
            "-C",
            "--git-dir",
            "--work-tree",
            "--exec-path",
            "--output",
            "--ext-diff",
            "--textconv",
        ]
        .iter()
        .any(|option| word == option || word.starts_with(&format!("{option}=")))
            || word.starts_with("-c")
            || word.starts_with("-C")
    }) {
        return true;
    }
    let Some(subcommand) = words.get(1) else {
        return true;
    };
    let args = &words[2..];
    match *subcommand {
        "status" | "diff" | "log" | "show" | "blame" | "rev-parse" | "ls-files" | "ls-tree"
        | "cat-file" | "describe" | "shortlog" | "grep" => false,
        "branch" => !args.iter().all(|arg| {
            matches!(
                *arg,
                "--list"
                    | "-l"
                    | "-a"
                    | "--all"
                    | "-r"
                    | "--remotes"
                    | "-v"
                    | "-vv"
                    | "--verbose"
                    | "--show-current"
            )
        }),
        "tag" => {
            !args.is_empty()
                && !args
                    .iter()
                    .all(|arg| matches!(*arg, "--list" | "-l" | "-n"))
        }
        "remote" => !args.iter().all(|arg| matches!(*arg, "-v" | "--verbose")),
        "config" => {
            !(matches!(
                args.first().copied(),
                Some("--get" | "--get-all" | "--get-regexp")
            ) && args.len() == 2
                || matches!(args.first().copied(), Some("--list" | "-l")) && args.len() == 1)
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn read_only_and_mutating_families_are_distinct() {
        for command in [
            "status",
            "diff",
            "log",
            "show",
            "blame",
            "rev-parse",
            "ls-files",
            "ls-tree",
            "cat-file",
            "describe",
            "shortlog",
            "grep",
            "branch",
            "branch -a",
            "tag --list",
            "remote -v",
            "config --get user.name",
            "config --list",
        ] {
            assert!(!forces_mutation(&format!("git {command}")), "{command}");
        }
        for command in [
            "commit",
            "push",
            "pull",
            "fetch",
            "merge",
            "rebase",
            "reset",
            "checkout",
            "switch",
            "restore",
            "clean",
            "stash",
            "tag new",
            "branch new",
            "branch -D old",
            "branch -m old new",
            "add",
            "rm",
            "mv",
            "apply",
            "am",
            "cherry-pick",
            "revert",
            "config x y",
            "config --unset x",
            "remote add x y",
            "remote remove x",
            "remote set-url x y",
            "submodule",
            "worktree add x",
            "worktree remove x",
            "gc",
            "prune",
            "update-ref",
            "notes",
            "-c x=y status",
            "-C /tmp status",
            "--config-env=x=y status",
            "--git-dir=x status",
            "--work-tree=x status",
            "--exec-path=x status",
            "diff --output=x",
            "diff --ext-diff",
            "show --textconv",
            "status > file",
            "status | cat",
            "unknown",
            "status 'quoted'",
        ] {
            assert!(forces_mutation(&format!("git {command}")), "{command}");
        }
    }
}
