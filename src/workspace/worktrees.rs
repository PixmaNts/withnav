//! The git worktrees of the workspace's repository, read with `git worktree list --porcelain -z`.
//! The permission gate treats them as part of the project: an agent working on parallel branches
//! writes in sibling directories of the checkout, and every such call would otherwise ask.
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::io::AsyncReadExt;

/// How long git may take before it is killed and no worktree is reported.
const DEADLINE: Duration = Duration::from_secs(2);
/// The most output read; a longer list is cut (the records read so far are kept).
const MAX_OUTPUT: u64 = 256 * 1024;

/// The other worktrees of the repository `workspace` belongs to, canonical where they exist.
/// Excludes the checkout that holds the workspace (it is already the project, and a workspace in a
/// subdirectory must not widen to its whole checkout), bare entries and prunable ones (the
/// directory is gone). Empty when git is missing, `workspace` is not in a repository, or git is
/// too slow.
pub async fn list(workspace: &Path) -> Vec<PathBuf> {
    let Some(output) = run(workspace).await else {
        return Vec::new();
    };
    let own = tokio::fs::canonicalize(workspace)
        .await
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut roots = Vec::new();
    for path in parse(&output) {
        roots.push(tokio::fs::canonicalize(&path).await.unwrap_or(path));
    }
    // The workspace's own checkout is the deepest listed worktree that holds it; an enclosing one
    // (a linked worktree nested under the main checkout) is another worktree and stays.
    let own_checkout = roots
        .iter()
        .enumerate()
        .filter(|(_, root)| own.starts_with(root))
        .max_by_key(|(_, root)| root.components().count())
        .map(|(at, _)| at);
    if let Some(at) = own_checkout {
        roots.remove(at);
    }
    roots
}

async fn run(workspace: &Path) -> Option<Vec<u8>> {
    let mut command = tokio::process::Command::new("git");
    let mut child = super::git::git_command(&mut command)
        .arg("-C")
        .arg(workspace)
        .args([
            "--no-optional-locks",
            "worktree",
            "list",
            "--porcelain",
            "-z",
        ])
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?.take(MAX_OUTPUT);
    let work = async {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await.ok()?;
        child.wait().await.ok()?.success().then_some(output)
    };
    // The child is killed when it is dropped, which the timeout does on expiry.
    tokio::time::timeout(DEADLINE, work).await.ok().flatten()
}

/// The paths of the usable worktrees in `git worktree list --porcelain -z` output: NUL-terminated
/// attribute records, a worktree's attributes ended by an empty record.
fn parse(output: &[u8]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut current: Option<PathBuf> = None;
    let mut skip = false;
    let mut finish = |current: &mut Option<PathBuf>, skip: &mut bool| {
        if let Some(path) = current.take().filter(|_| !*skip) {
            paths.push(path);
        }
        *skip = false;
    };
    for record in output.split(|byte| *byte == 0) {
        let Ok(record) = std::str::from_utf8(record) else {
            // A path that is not UTF-8 cannot be matched against the gate's text paths.
            current = None;
            skip = true;
            continue;
        };
        if record.is_empty() {
            finish(&mut current, &mut skip);
        } else if let Some(path) = record.strip_prefix("worktree ") {
            finish(&mut current, &mut skip);
            current = Some(PathBuf::from(path));
        } else if record == "bare" || record == "prunable" || record.starts_with("prunable ") {
            skip = true;
        }
    }
    finish(&mut current, &mut skip);
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(records: &[&str]) -> Vec<u8> {
        records
            .iter()
            .flat_map(|r| [r.as_bytes(), &[0]].concat())
            .collect()
    }

    #[test]
    fn parses_every_usable_worktree() {
        let out = z(&[
            "worktree /dev/Navai",
            "HEAD abc",
            "branch refs/heads/develop",
            "",
            "worktree /dev/Navai-wt/with space",
            "HEAD def",
            "detached",
            "locked in use",
            "",
            "worktree /dev/Navai-wt/gone",
            "HEAD 123",
            "prunable gitdir file points to non-existent location",
            "",
            "worktree /dev/Navai.git",
            "bare",
            "",
        ]);
        assert_eq!(
            parse(&out),
            [
                PathBuf::from("/dev/Navai"),
                PathBuf::from("/dev/Navai-wt/with space")
            ]
        );
    }

    #[test]
    fn a_missing_final_blank_record_keeps_the_last_worktree() {
        assert_eq!(parse(&z(&["worktree /a", "HEAD 1"])), [PathBuf::from("/a")]);
        assert!(parse(b"").is_empty());
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[tokio::test]
    async fn lists_the_other_worktrees_and_keeps_an_enclosing_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "--quiet"]);
        git(&main, &["commit", "--quiet", "--allow-empty", "-m", "base"]);
        // One worktree beside the checkout, one nested under it.
        let beside = root.join("beside");
        let nested = main.join("nested");
        for (path, branch) in [(&beside, "b"), (&nested, "n")] {
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
        }
        std::fs::create_dir_all(main.join("src")).unwrap();

        // From the main checkout (or a subdirectory of it) its own root is not listed.
        let mut from_main = list(&main).await;
        from_main.sort();
        assert_eq!(from_main, [beside.clone(), nested.clone()]);
        assert_eq!(list(&main.join("src")).await.len(), 2);
        // From the nested worktree, the enclosing main checkout stays: it is another worktree.
        let mut from_nested = list(&nested).await;
        from_nested.sort();
        assert_eq!(from_nested, [beside.clone(), main.clone()]);
        // Outside any repository, or in a folder that does not exist: nothing.
        assert!(list(&root.join("plain")).await.is_empty());
        std::fs::create_dir_all(root.join("plain")).unwrap();
        assert!(list(&root.join("plain")).await.is_empty());
    }
}
