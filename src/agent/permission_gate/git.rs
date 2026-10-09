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
    (words.first() == Some(&"git") && words.get(1).is_some_and(|word| !word.starts_with('-')))
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
            "-f" | "--force" | "--force-with-lease" => force = true,
            "-d" | "--delete" => delete = true,
            "-u" | "--set-upstream" | "--no-verify" | "--porcelain" | "--verbose" | "-v" => {}
            option if option.starts_with("--force-with-lease=") => force = true,
            option if option.starts_with('-') => understood = false,
            operand => positional.push(operand),
        }
    }
    let remote = positional.first().copied();
    let refspec = positional.get(1).copied();
    force |= refspec.is_some_and(|spec| spec.starts_with('+'));
    delete |= refspec.is_some_and(|spec| spec.starts_with(':'));
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
        "literal_target":understood && positional.len() == 2 && remote == Some("origin") && target.is_some_and(|branch| !branch.is_empty() && !branch.starts_with("refs/")),
        "own_branch":own_branch}),
    )
}

async fn read(workspace: &Path, args: &[&str]) -> Option<String> {
    let mut child = tokio::process::Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
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
    if output.len() > 8192 || !child.wait().await.ok()?.success() {
        return None;
    }
    String::from_utf8(output)
        .ok()
        .map(|text| text.trim().to_owned())
}

pub(super) async fn facts(command: &str, workspace: &Path) -> Option<Value> {
    let words = literal(command)?;
    tokio::time::timeout(Duration::from_millis(300), async {
        let current = read(workspace, &["symbolic-ref", "--short", "HEAD"]).await;
        let default = read(workspace, &["symbolic-ref", "refs/remotes/origin/HEAD"]).await
            .and_then(|branch| branch.strip_prefix("refs/remotes/origin/").map(str::to_owned));
        let dirty = read(workspace, &["status", "--porcelain", "--untracked-files=normal"]).await.map(|s| !s.is_empty());
        let mut tracked_paths = serde_json::Map::new();
        // Only literal path operands; options and refspecs are not paths.
        if matches!(words[1], "add" | "rm" | "restore" | "checkout" | "diff") {
            for path in words.iter().skip(2).filter(|word| !word.starts_with('-')).take(16) {
                let tracked = read(workspace, &["ls-files", "--error-unmatch", "--", path]).await.is_some();
                tracked_paths.insert((*path).into(), json!(tracked));
            }
        }
        let push = push(&words, current.as_deref());
        let non_default_push = push.as_ref().is_some_and(|push| {
            push["literal_target"] == true && push["own_branch"] == true
                && push["force"] == false && push["delete"] == false
                && default.as_ref().is_some_and(|default| push["branch"].as_str().is_some_and(|target| target != default))
        });
        Some(json!({"current_branch":current,"default_branch":default,
            "current_is_default":current.as_ref().zip(default.as_ref()).map(|(a,b)| a == b),
            "dirty":dirty,"tracked_paths":tracked_paths,"push":push,"non_default_push":non_default_push}))
    }).await.ok().flatten()
}
