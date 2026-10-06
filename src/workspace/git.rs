//! What the header and the Changes panel show of the workspace's git state: the branch and the
//! changed files. Read with `git status -z`, off the reducer, streamed and bounded, and parsed here.
use std::{path::Path, process::Stdio, time::Duration};

use tokio::io::{AsyncBufReadExt, BufReader};

/// The changed files shown at most; the rest is counted.
pub const MAX_FILES: usize = 10;
/// How long a read may take before git is killed and nothing is shown.
pub const DEADLINE: Duration = Duration::from_secs(5);
/// The longest record kept (a path), in bytes; a longer one is counted, not shown.
const MAX_RECORD: usize = 4096;

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

/// Builds a [`GitStatus`] one `-z` record at a time, keeping only what is shown.
#[derive(Debug, Default)]
pub struct Parser {
    status: GitStatus,
    /// The next record is the old path of a rename or copy: skipped.
    skip_source: bool,
}

impl Parser {
    /// One NUL-terminated record of `git status --porcelain=v1 --branch -z`, without its NUL.
    /// `None` for a record too long to keep (still counted when it is a file).
    pub fn record(&mut self, record: Option<&[u8]>) {
        if std::mem::take(&mut self.skip_source) {
            return;
        }
        let Some(record) = record else {
            self.status.dirty += 1;
            return;
        };
        if let Some(head) = record.strip_prefix(b"## ") {
            self.status.branch = branch(&String::from_utf8_lossy(head));
            return;
        }
        // `XY path`: two status letters, a space, the path as it is on disk (no quoting with -z).
        if record.len() < 4 || record[2] != b' ' {
            return;
        }
        let code = &record[..2];
        // A rename or a copy is followed by a record holding the old path.
        self.skip_source = code.contains(&b'R') || code.contains(&b'C');
        self.status.dirty += 1;
        if self.status.files.len() >= MAX_FILES {
            return;
        }
        let mark = match code {
            b"??" => '?',
            c if c.contains(&b'D') => 'D',
            c if c.contains(&b'A') => 'A',
            c if c.contains(&b'R') || c.contains(&b'C') => 'R',
            _ => 'M',
        };
        self.status.files.push(Change {
            mark,
            path: escape(&String::from_utf8_lossy(&record[3..])),
        });
    }

    pub fn finish(self) -> GitStatus {
        self.status
    }
}

/// The branch from a `## ` header: `main...origin/main [ahead 1]`, `No commits yet on main`, or
/// `HEAD (no branch)` for a detached head.
fn branch(head: &str) -> Option<String> {
    if head.starts_with("HEAD (no branch)") {
        return None;
    }
    let head = head.strip_prefix("No commits yet on ").unwrap_or(head);
    let name = head.split("...").next().unwrap_or(head);
    // Without an upstream the header may still end with ` [gone]` and the like.
    let name = name.split(" [").next().unwrap_or(name);
    Some(escape(name))
}

/// Parses the whole of `git status --porcelain=v1 --branch -z` output.
pub fn parse(output: &[u8]) -> GitStatus {
    let mut parser = Parser::default();
    for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        parser.record(Some(record));
    }
    parser.finish()
}

/// `text` with control characters written out (`\u{1b}`): a file or branch name cannot drive the
/// terminal or break a row.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// Reads the git state of `workspace`; `None` when it is not in a repository, git is missing, or
/// it took longer than [`DEADLINE`] (git is then killed).
pub async fn read(workspace: &Path) -> Option<GitStatus> {
    read_with(Path::new("git"), workspace, DEADLINE).await
}

/// [`read`] with the program and the deadline given: tests use a stand-in that hangs.
pub async fn read_with(program: &Path, workspace: &Path, deadline: Duration) -> Option<GitStatus> {
    let mut command = tokio::process::Command::new(program);
    read_command(&mut command, workspace, deadline).await
}

async fn read_command(
    command: &mut tokio::process::Command,
    workspace: &Path,
    deadline: Duration,
) -> Option<GitStatus> {
    let child = command
        .arg("-C")
        .arg(workspace)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--branch",
            "-z",
            "--untracked-files=normal",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    read_child(child, deadline).await
}

async fn read_child(mut child: tokio::process::Child, deadline: Duration) -> Option<GitStatus> {
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    let work = async {
        // Error output is not shown: it is read and thrown away as it comes, so nothing of it is
        // kept and a chatty git never blocks on a full pipe.
        let drain = async {
            let mut stderr = stderr;
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        };
        let parse = async {
            let mut parser = Parser::default();
            let mut reader = BufReader::new(stdout);
            let mut record = Vec::new();
            loop {
                record.clear();
                // Reads one record, keeping at most `MAX_RECORD` bytes of it.
                let mut too_long = false;
                let mut ended = false;
                loop {
                    let chunk = match reader.fill_buf().await {
                        Ok(chunk) => chunk,
                        Err(_) => return None,
                    };
                    if chunk.is_empty() {
                        ended = true;
                        break;
                    }
                    let (part, found) = match chunk.iter().position(|b| *b == 0) {
                        Some(at) => (&chunk[..at], Some(at)),
                        None => (chunk, None),
                    };
                    if record.len() + part.len() <= MAX_RECORD {
                        record.extend_from_slice(part);
                    } else {
                        too_long = true;
                    }
                    let used = found.map_or(part.len(), |at| at + 1);
                    reader.consume(used);
                    if found.is_some() {
                        break;
                    }
                }
                if ended && record.is_empty() && !too_long {
                    break;
                }
                parser.record((!too_long).then_some(&record[..]));
                if ended {
                    break;
                }
            }
            Some(parser)
        };
        let (parser, ()) = tokio::join!(parse, drain);
        let status = child.wait().await.ok()?;
        status
            .success()
            .then(|| parser.map(Parser::finish))
            .flatten()
    };
    match tokio::time::timeout(deadline, work).await {
        Ok(status) => status,
        Err(_) => {
            // Reap after killing rather than leaving cleanup to Tokio's orphan queue.
            let _ = child.kill().await;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(records: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for r in records {
            out.extend_from_slice(r.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn branch_counts_and_marks() {
        let s = parse(&z(&[
            "## develop...origin/develop [ahead 1]",
            " M src/a.rs",
            "A  src/b.rs",
            " D old.rs",
            "R  y.rs",
            "x.rs",
            "?? new.txt",
        ]));
        assert_eq!(s.branch.as_deref(), Some("develop"));
        assert_eq!(s.dirty, 5, "the old path of a rename is not a change");
        let marks: String = s.files.iter().map(|f| f.mark).collect();
        assert_eq!(marks, "MADR?");
        assert_eq!(s.files[3].path, "y.rs");
    }

    #[test]
    fn names_are_kept_as_they_are_on_disk() {
        let s = parse(&z(&[
            "## main",
            " M 日本語/ファイル.rs",
            "?? \"quoted\" name.txt",
            "??   leading spaces.txt",
            "?? a -> b.txt",
            "R  new -> name.txt",
            "old -> name.txt",
            "?? bell\u{7}.txt",
        ]));
        let paths: Vec<&str> = s.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "日本語/ファイル.rs",
                "\"quoted\" name.txt",
                "  leading spaces.txt",
                "a -> b.txt",
                "new -> name.txt",
                "bell\\u{7}.txt",
            ]
        );
        assert_eq!(s.dirty, 6);
    }

    #[test]
    fn detached_fresh_gone_and_many() {
        assert_eq!(parse(&z(&["## HEAD (no branch)"])).branch, None);
        assert_eq!(
            parse(&z(&["## No commits yet on main"])).branch.as_deref(),
            Some("main")
        );
        assert_eq!(
            parse(&z(&["## topic [gone]"])).branch.as_deref(),
            Some("topic")
        );
        let many: Vec<String> = (0..25).map(|n| format!(" M f{n}")).collect();
        let mut records = vec!["## main"];
        records.extend(many.iter().map(String::as_str));
        let s = parse(&z(&records));
        assert_eq!((s.dirty, s.files.len()), (25, MAX_FILES));
    }

    #[cfg(unix)]
    fn fake_git(dir: &Path, body: &str) -> tokio::process::Command {
        let path = dir.join("git");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        // In October 2026, parallel subprocess forks could inherit an open writer while the
        // fixture was being written, making direct execution fail with ETXTBSY. Read it instead.
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg(path);
        command
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_git_that_hangs_is_killed_at_the_deadline() {
        // In October 2026, the deadline could kill the fixture before it wrote its pid file.
        // The parent already knows the pid; a virtual clock tests the deadline, not scheduling.
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let deadline = Duration::from_millis(300);
        let started = tokio::time::Instant::now();
        let read = read_child(child, deadline).await;
        assert_eq!(read, None);
        assert_eq!(started.elapsed(), deadline, "returned at the deadline");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "git was killed and reaped at the deadline"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_large_output_is_counted_and_only_the_first_files_kept() {
        let dir = tempfile::tempdir().unwrap();
        // 50 000 changed files and 1 MB of error output: read through, nothing buffered whole.
        let mut git = fake_git(
            dir.path(),
            "printf '## main\\0'; i=0; while [ $i -lt 50000 ]; do printf ' M file_%d\\0' $i; i=$((i+1)); done; \
             head -c 1000000 /dev/zero >&2",
        );
        let inherited_writer = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("git"))
            .unwrap();
        let read = read_command(&mut git, dir.path(), Duration::from_secs(60))
            .await
            .expect("a status");
        drop(inherited_writer);
        assert_eq!(read.dirty, 50_000);
        assert_eq!(read.files.len(), MAX_FILES);
        assert_eq!(read.files[0].path, "file_0");
        assert_eq!(read.branch.as_deref(), Some("main"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_git_shows_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut git = fake_git(
            dir.path(),
            "echo 'fatal: not a git repository' >&2; exit 128",
        );
        assert_eq!(read_command(&mut git, dir.path(), DEADLINE).await, None);
    }
}
