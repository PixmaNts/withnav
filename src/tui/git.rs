//! What the header and the Changes panel show of the workspace's git state: the branch and the
//! changed files. Read with `git status`, off the reducer, and parsed here.
use std::path::Path;

/// The changed files shown at most; the rest is counted.
pub const MAX_FILES: usize = 10;

/// One changed file: `M`, `A`, `D`, `R` or `?` (untracked), and its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub mark: char,
    pub path: String,
}

/// The workspace's branch and changes, as `git status` reported them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitStatus {
    /// The branch name; `None` on a detached head.
    pub branch: Option<String>,
    /// How many files differ from the last commit (untracked included).
    pub dirty: usize,
    /// The first [`MAX_FILES`] of them.
    pub files: Vec<Change>,
}

/// Parses `git status --porcelain=v1 --branch` output.
pub fn parse(output: &str) -> GitStatus {
    let mut status = GitStatus::default();
    for line in output.lines() {
        if let Some(head) = line.strip_prefix("## ") {
            let name = head.split("...").next().unwrap_or(head);
            let name = name.strip_prefix("No commits yet on ").unwrap_or(name);
            if !name.starts_with("HEAD (no branch)") {
                status.branch = Some(clean(name.split(' ').next().unwrap_or(name)));
            }
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        status.dirty += 1;
        if status.files.len() >= MAX_FILES {
            continue;
        }
        let (code, path) = line.split_at(2);
        let mark = match code {
            "??" => '?',
            c if c.contains('D') => 'D',
            c if c.contains('A') => 'A',
            c if c.contains('R') => 'R',
            _ => 'M',
        };
        let path = path.trim_start();
        // A rename shows `old -> new`: the new path is what is there now.
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        status.files.push(Change {
            mark,
            path: clean(path.trim_matches('"')),
        });
    }
    status
}

/// `text` without control characters: a file or branch name cannot drive the terminal.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Reads the git state of `workspace`; `None` when it is not in a repository or git is missing.
pub async fn read(workspace: &Path) -> Option<GitStatus> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), output)
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_counts_and_marks() {
        let s = parse(
            "## develop...origin/develop [ahead 1]\n M src/a.rs\nA  src/b.rs\n D old.rs\nR  x.rs -> y.rs\n?? new.txt\n",
        );
        assert_eq!(s.branch.as_deref(), Some("develop"));
        assert_eq!(s.dirty, 5);
        let marks: String = s.files.iter().map(|f| f.mark).collect();
        assert_eq!(marks, "MADR?");
        assert_eq!(s.files[3].path, "y.rs");
    }

    #[test]
    fn detached_fresh_and_many() {
        assert_eq!(parse("## HEAD (no branch)\n").branch, None);
        assert_eq!(
            parse("## No commits yet on main\n").branch.as_deref(),
            Some("main")
        );
        let many: String = (0..25).map(|n| format!(" M f{n}\n")).collect();
        let s = parse(&format!("## main\n{many}"));
        assert_eq!((s.dirty, s.files.len()), (25, MAX_FILES));
    }
}
