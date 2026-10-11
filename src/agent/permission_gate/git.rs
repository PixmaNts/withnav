//! Bounded read-only host facts for literal git calls. No shell or git aliases are executed.
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

pub(super) fn literal(command: &str) -> Option<Vec<&str>> {
    // Deliberately narrow: quoted, expanded, chained and redirected calls get no facts.
    if command.contains([
        '\'', '"', '$', '`', ';', '|', '&', '>', '<', '\n', '\\', '*', '?', '(', ')', '{', '}',
    ]) {
        return None;
    }
    let words: Vec<_> = command.split_whitespace().collect();
    (words
        .first()
        .is_some_and(|word| matches!(*word, "git" | "/usr/bin/git"))
        && words.get(1).is_some_and(|word| !word.starts_with('-')))
    .then_some(words)
}

pub(super) fn push(words: &[&str], current: Option<&str>) -> Option<Value> {
    if words.get(1) != Some(&"push") {
        return None;
    }
    let mut force = false;
    let mut delete = false;
    let mut positional = Vec::new();
    let mut understood = true;
    for word in &words[2..] {
        match *word {
            option if option.starts_with('-') && force_push_operand(option) => force = true,
            option if option.starts_with('-') && delete_push_operand(option) => delete = true,
            option if safe_push_option(option) => {}
            option if option.starts_with('-') => understood = false,
            operand => positional.push(operand),
        }
    }
    let remote = positional.first().copied();
    let refspec = positional.get(1).copied();
    force |= positional.iter().any(|spec| force_push_operand(spec));
    delete |= positional.iter().any(|spec| delete_push_operand(spec));
    // An omitted refspec depends on push.default / remote configuration: never guess.
    let target = refspec.map(|spec| {
        let spec = spec.trim_start_matches('+');
        spec.split_once(':')
            .map_or(spec, |(_, destination)| destination)
            .trim_start_matches("refs/heads/")
    });
    let source = refspec.map(|spec| spec.trim_start_matches('+').split(':').next().unwrap_or(""));
    let own_branch = source.is_some_and(|source| {
        source == "HEAD" || Some(source.trim_start_matches("refs/heads/")) == current
    }) && target == current;
    Some(
        json!({"remote":remote,"branch":target,"force":force,"delete":delete,
        "literal_target":understood && !force && !delete && positional.len() == 2 && remote == Some("origin") && target.is_some_and(|branch| !branch.is_empty() && !branch.starts_with("refs/")),
        "own_branch":own_branch && !force && !delete}),
    )
}

pub(super) fn safe_push_option(word: &str) -> bool {
    matches!(
        word,
        "-u" | "--set-upstream" | "--no-verify" | "--porcelain" | "--verbose" | "-v"
    )
}

pub(super) fn captain_only_push_operand(word: &str) -> bool {
    (word.starts_with('-') && !safe_push_option(word))
        || force_push_operand(word)
        || delete_push_operand(word)
        || broad_push_operand(word)
}

pub(super) fn force_push_operand(word: &str) -> bool {
    matches!(word, "-f" | "--force" | "--force-with-lease")
        || word.starts_with("--force-with-lease=")
        || word.starts_with('+')
}

pub(super) fn delete_push_operand(word: &str) -> bool {
    matches!(word, "-d" | "--delete") || word.trim_start_matches('+').starts_with(':')
}

pub(super) fn broad_push_operand(word: &str) -> bool {
    matches!(word, "--all" | "--mirror" | "--tags" | "--follow-tags")
}

async fn read(workspace: &Path, args: &[&str]) -> Option<String> {
    // Never resolve a pre-approval executable through the workspace/PATH. No ambient
    // GIT_* override, global configuration, fsmonitor or hook may execute here.
    let mut command = tokio::process::Command::new("/usr/bin/git");
    crate::workspace::git::git_command(&mut command);
    command.env_clear();
    // Config and remote get-url only parse data and must include effective
    // publication settings (including global URL rewrites). Ref/index probes
    // isolate global/system settings entirely.
    if args.first() == Some(&"config") || args.starts_with(&["remote", "get-url"]) {
        for key in ["HOME", "XDG_CONFIG_HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    } else {
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
    }
    let mut child = command
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.untrackedCache=false",
        ])
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut output = Vec::new();
    child
        .stdout
        .take()?
        .take(8193)
        .read_to_end(&mut output)
        .await
        .ok()?;
    let status = child.wait().await.ok()?;
    // config --get-all reports absent keys with exit 1; all other failures
    // remain unknown, not an empty/safe configuration.
    let absent_config = args.first() == Some(&"config")
        && args.contains(&"--get-all")
        && status.code() == Some(1)
        && output.is_empty();
    if output.len() > 8192 || (!status.success() && !absent_config) {
        return None;
    }
    String::from_utf8(output)
        .ok()
        .map(|text| text.trim().to_owned())
}

pub(super) async fn facts(command: &str, cx: &crate::tools::ToolContext) -> Option<Value> {
    let words = literal(command)?;
    super::shell::trusted_program(
        words[0],
        &json!({"workspace":cx.workspace,"writable_roots":cx.writable_roots}),
    )?;
    let workspace = &cx.workspace;
    tokio::time::timeout(Duration::from_millis(300), async {
        let current = read(workspace, &["symbolic-ref", "--short", "HEAD"]).await;
        let default = read(workspace, &["symbolic-ref", "refs/remotes/origin/HEAD"])
            .await
            .and_then(|branch| {
                branch
                    .strip_prefix("refs/remotes/origin/")
                    .map(str::to_owned)
            });
        let push = push(&words, current.as_deref());
        let push_urls = if let Some(remote) = push.as_ref().and_then(|p| p["remote"].as_str()) {
            read(workspace, &["remote", "get-url", "--push", "--all", remote])
                .await
                .filter(|text| !text.is_empty())
                .map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
        } else {
            None
        };
        let follow_tags = read(
            workspace,
            &[
                "config",
                "--type=bool",
                "--default=false",
                "--get",
                "push.followTags",
            ],
        )
        .await;
        let mirror = read(
            workspace,
            &[
                "config",
                "--type=bool",
                "--default=false",
                "--get",
                "remote.origin.mirror",
            ],
        )
        .await;
        let remote_push = read(workspace, &["config", "--get-all", "remote.origin.push"]).await;
        let push_default = read(
            workspace,
            &["config", "--default=simple", "--get", "push.default"],
        )
        .await;
        let safe_config = follow_tags.as_deref() == Some("false")
            && mirror.as_deref() == Some("false")
            && remote_push.as_deref() == Some("")
            && matches!(
                push_default.as_deref(),
                Some("simple" | "current" | "upstream" | "nothing")
            );
        let non_default_push = push.as_ref().is_some_and(|push| {
            safe_config
                && push["literal_target"] == true
                && push["own_branch"] == true
                && push["force"] == false
                && push["delete"] == false
                && default.as_ref().is_some_and(|default| {
                    push["branch"]
                        .as_str()
                        .is_some_and(|target| target != default)
                })
        });
        Some(json!({"current_branch":current,"default_branch":default,
            "current_is_default":current.as_ref().zip(default.as_ref()).map(|(a,b)| a == b),
            "safe_push_config":safe_config,"push":push,"push_urls":push_urls,"non_default_push":non_default_push}))
    })
    .await
    .ok()
    .flatten()
}

/// A tracked path alone is not a recovery copy: both index and working tree
/// must match HEAD. Do not use diff here: even --no-textconv/--no-ext-diff
/// can invoke repository-controlled clean filters when reading the working tree.
pub(super) async fn recoverable_file(workspace: &Path, path: &str) -> bool {
    let Ok(workspace) = workspace.canonicalize() else {
        return false;
    };
    let target = workspace.join(path);
    let Ok(metadata) = std::fs::symlink_metadata(&target) else {
        return false;
    };
    if !metadata.is_file() || target.canonicalize().ok().as_ref() != Some(&target) {
        return false;
    }
    let Ok(relative) = target.strip_prefix(&workspace) else {
        return false;
    };
    let Some(relative) = relative.to_str() else {
        return false;
    };
    let head_ref = format!("HEAD:{relative}");
    tokio::time::timeout(Duration::from_millis(300), async {
        let Some(index) = read(
            &workspace,
            &["ls-files", "--stage", "--error-unmatch", "--", relative],
        )
        .await
        else {
            return false;
        };
        let Some(head) = read(&workspace, &["rev-parse", "--verify", &head_ref]).await else {
            return false;
        };
        let Some(working) =
            read(&workspace, &["hash-object", "--no-filters", "--", relative]).await
        else {
            return false;
        };
        let mut fields = index.split_whitespace();
        matches!(fields.next(), Some("100644" | "100755"))
            && fields.next() == Some(head.as_str())
            && fields.next() == Some("0")
            && head == working
    })
    .await
    .unwrap_or(false)
}
