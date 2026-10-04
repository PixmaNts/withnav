//! Ignore-aware bounded grep. Sensitive exclusions are applied before opening files.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use super::{bound_output, ToolResult, DEFAULT_MAX_OUTPUT_BYTES};
use crate::agent::permission_gate::{sensitive_read, SENSITIVE_NAMES};
use crate::error::ToolError;

const DEADLINE: Duration = Duration::from_secs(5);
const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 32;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MATCHES: usize = 1000;

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    depth: usize,
    file_bytes: u64,
    total_bytes: u64,
    matches: usize,
    duration: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: MAX_ENTRIES,
            depth: MAX_DEPTH,
            file_bytes: MAX_FILE_BYTES,
            total_bytes: MAX_TOTAL_BYTES,
            matches: MAX_MATCHES,
            duration: DEADLINE,
        }
    }
}

fn result(
    mut text: String,
    engine: &str,
    skipped: usize,
    partial: Option<&str>,
    visited: usize,
) -> ToolResult {
    if skipped > 0 {
        text.push_str(&format!(
            "\n{skipped} sensitive files skipped; read them individually, each read is reviewed"
        ));
    }
    if let Some(reason) = partial {
        text.push_str(&format!("\n[partial results: {reason}]"));
    }
    let (text, truncated) = bound_output(&text, DEFAULT_MAX_OUTPUT_BYTES);
    let mut result = ToolResult::ok(text).with_metadata(
        json!({"engine":engine,"sensitive_skipped":skipped,"partial":partial,"visited":visited}),
    );
    if let Some(bytes) = truncated {
        result = result.with_truncation(bytes);
    }
    result
}

pub(super) fn rg_command(pattern: &str, root: &Path) -> tokio::process::Command {
    rg_command_with_hidden(pattern, root, false)
}

fn rg_command_with_hidden(pattern: &str, root: &Path, hidden: bool) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("rg");
    command
        .kill_on_drop(true)
        .arg("--no-config")
        .arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never");
    if hidden {
        command.arg("--hidden");
    }
    for name in SENSITIVE_NAMES {
        command.arg("--iglob").arg(format!("!**/{name}"));
        command.arg("--iglob").arg(format!("!**/{name}/**"));
    }
    command.arg("--regexp").arg(pattern).arg("--").arg(root);
    command
}

fn rg_available() -> bool {
    std::env::var_os("NAVAI_FORCE_BUILTIN_GREP").is_none()
        && std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("rg").is_file()))
}

pub(super) async fn search(
    pattern: String,
    root: PathBuf,
    cancel: CancellationToken,
) -> Result<ToolResult, ToolError> {
    if sensitive_read(&root) {
        return Ok(result(String::new(), "builtin", 1, None, 0));
    }
    if rg_available() {
        rg_search(&pattern, &root, &cancel).await
    } else {
        builtin(pattern, root, cancel, Limits::default()).await
    }
}

async fn rg_search(
    pattern: &str,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<ToolResult, ToolError> {
    let mut command = rg_command(pattern, root);
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let errors = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr
            .take(4096)
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
    });
    let mut bytes = Vec::new();
    let mut bounded_stdout = (&mut stdout).take((DEFAULT_MAX_OUTPUT_BYTES + 1) as u64);
    let read = tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        read = tokio::time::timeout(DEADLINE, bounded_stdout.read_to_end(&mut bytes)) => Some(read),
    };
    let partial = if read.is_none() {
        Some("cancelled")
    } else if matches!(read, Some(Err(_))) {
        Some("timeout")
    } else if bytes.len() > DEFAULT_MAX_OUTPUT_BYTES {
        Some("output limit")
    } else {
        None
    };
    if partial.is_some() {
        child.kill().await?;
    }
    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => { child.kill().await?; errors.abort(); return Ok(ToolResult::interrupted()); },
        status = tokio::time::timeout(DEADLINE, child.wait()) => match status {
            Ok(status) => status?,
            Err(_) => { child.kill().await?; errors.abort(); return Ok(result(String::from_utf8_lossy(&bytes).into_owned(), "rg", 0, Some("timeout"), 0)); },
        }
    };
    let stderr = errors
        .await
        .map_err(|error| ToolError::Io(std::io::Error::other(error)))??;
    if partial == Some("cancelled") {
        return Ok(ToolResult::interrupted());
    }
    if let Some(Ok(Err(error))) = read {
        return Err(ToolError::Io(error));
    }
    if !status.success() && status.code() != Some(1) && partial.is_none() {
        return Ok(ToolResult::error(
            "grep_failed",
            String::from_utf8_lossy(&stderr),
        ));
    }
    // Defense in depth only: rg exclusions above prevented sensitive opens.
    let text = String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| {
            line.split_once(':')
                .is_none_or(|(path, _)| !sensitive_read(Path::new(path)))
        })
        .take(MAX_MATCHES)
        .collect::<Vec<_>>()
        .join("\n");
    let token = cancel.child_token();
    let _count_guard = token.clone().drop_guard();
    let count_root = root.to_path_buf();
    let counted = tokio::task::spawn_blocking(move || count_sensitive(&count_root, &token));
    let skipped = tokio::time::timeout(DEADLINE, counted)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(0);
    Ok(result(text, "rg", skipped, partial, 0))
}

fn builder(
    root: &Path,
    hidden: bool,
    depth: usize,
    skipped: Arc<AtomicUsize>,
) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(hidden)
        .max_depth(Some(depth))
        .follow_links(false)
        .filter_entry(move |entry| {
            if sensitive_read(entry.path()) {
                skipped.fetch_add(1, Ordering::Relaxed);
                false
            } else {
                true
            }
        });
    builder.add_custom_ignore_filename(".rgignore");
    builder
}

fn count_sensitive(root: &Path, cancel: &CancellationToken) -> usize {
    let skipped = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    for _ in builder(root, false, MAX_DEPTH, skipped.clone())
        .build()
        .take(MAX_ENTRIES)
    {
        if cancel.is_cancelled() || started.elapsed() >= DEADLINE {
            break;
        }
    }
    skipped.load(Ordering::Relaxed)
}

async fn builtin(
    pattern: String,
    root: PathBuf,
    cancel: CancellationToken,
    limits: Limits,
) -> Result<ToolResult, ToolError> {
    let worker_cancel = cancel.child_token();
    let _guard = worker_cancel.clone().drop_guard();
    let token = worker_cancel.clone();
    let worker = tokio::task::spawn_blocking(move || walk(&pattern, &root, &token, limits));
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Ok(ToolResult::interrupted()),
        timed = tokio::time::timeout(limits.duration + Duration::from_millis(500), worker) => match timed {
            Ok(joined) => joined.map_err(|error| ToolError::Io(std::io::Error::other(error)))?,
            Err(_) => Ok(result(String::new(), "builtin", 0, Some("timeout"), 0)),
        }
    }
}

fn walk(
    pattern: &str,
    root: &Path,
    cancel: &CancellationToken,
    limits: Limits,
) -> Result<ToolResult, ToolError> {
    if cancel.is_cancelled() {
        return Ok(ToolResult::interrupted());
    }
    let regex = regex::Regex::new(pattern).map_err(|error| ToolError::Denied {
        code: "invalid_regex".into(),
        message: error.to_string(),
    })?;
    let skipped = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let mut text = String::new();
    let mut visited = 0;
    let mut bytes_read = 0;
    let mut matches = 0;
    let mut partial = None;
    'walk: for entry in builder(root, true, limits.depth, skipped.clone()).build() {
        if cancel.is_cancelled() {
            return Ok(ToolResult::interrupted());
        }
        if started.elapsed() >= limits.duration {
            partial = Some("timeout");
            break;
        }
        if visited >= limits.entries {
            partial = Some("entry limit");
            break;
        }
        visited += 1;
        let Ok(entry) = entry else {
            continue;
        };
        if entry.depth() == limits.depth && entry.file_type().is_some_and(|kind| kind.is_dir()) {
            partial = Some("depth limit");
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) || sensitive_read(entry.path()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > limits.file_bytes {
            partial = Some("file size limit");
            continue;
        }
        if bytes_read + metadata.len() > limits.total_bytes {
            partial = Some("total byte limit");
            break;
        }
        let Ok(file) = std::fs::File::open(entry.path()) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take(limits.file_bytes + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        if bytes.len() as u64 > limits.file_bytes {
            partial = Some("file size limit");
            continue;
        }
        bytes_read += bytes.len() as u64;
        for (line, content) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if cancel.is_cancelled() {
                return Ok(ToolResult::interrupted());
            }
            if started.elapsed() >= limits.duration {
                partial = Some("timeout");
                break 'walk;
            }
            if regex.is_match(content) {
                text.push_str(&format!(
                    "{}:{}:{}\n",
                    entry.path().display(),
                    line + 1,
                    content
                ));
                matches += 1;
                if matches >= limits.matches {
                    partial = Some("match limit");
                    break 'walk;
                }
                if text.len() >= DEFAULT_MAX_OUTPUT_BYTES {
                    partial = Some("output limit");
                    break 'walk;
                }
            }
        }
    }
    let count = skipped.load(Ordering::Relaxed);
    Ok(result(text, "builtin", count, partial, visited))
}

#[cfg(test)]
pub(super) fn builtin_sync(pattern: &str, root: &Path) -> Result<ToolResult, ToolError> {
    walk(pattern, root, &CancellationToken::new(), Limits::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "manual real-worktree timing"]
    async fn real_worktree_directory_grep_timing() {
        use crate::tools::Tool;
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cx = crate::tools::ToolContext {
            workspace: root.clone(),
            writable_roots: vec![root.clone()],
            readable_roots: vec![],
            binding: None,
        };
        let started = Instant::now();
        let result = crate::tools::native::GrepTool
            .call(
                json!({"path":root,"pattern":"session"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        println!(
            "directory grep tool: {} ms; engine {}; output {} bytes",
            started.elapsed().as_millis(),
            result.metadata["engine"],
            result.output.len()
        );
        assert_eq!(result.status, crate::tools::ToolStatus::Ok);
        assert_eq!(result.metadata["engine"], "rg");
        let output = tokio::process::Command::new("rg")
            .arg("--debug")
            .arg("--files")
            .arg(&root)
            .output()
            .await
            .unwrap();
        let debug = String::from_utf8_lossy(&output.stderr);
        assert!(
            debug
                .lines()
                .any(|line| line.contains("target") && line.contains("ignoring")),
            "target ignore evidence missing"
        );
        assert!(!String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.starts_with(root.join("target").to_string_lossy().as_ref())));
        println!("target/ excluded by rg ignore rules before traversal");
    }
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join(".gitignore"),
            "target/\nignored.txt\n*.log\n!keep.log\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join(".ignore"), "other.txt\n").unwrap();
        std::fs::write(tmp.path().join(".rgignore"), "rg-only.txt\n").unwrap();
        for name in [
            "ordinary.txt",
            "ignored.txt",
            "drop.log",
            "keep.log",
            ".hidden",
            "other.txt",
            "rg-only.txt",
        ] {
            std::fs::write(tmp.path().join(name), "needle").unwrap();
        }
        std::fs::create_dir(tmp.path().join("target")).unwrap();
        for i in 0..1000 {
            std::fs::write(tmp.path().join(format!("target/{i}.txt")), "needle").unwrap();
        }
        tmp
    }

    #[tokio::test]
    async fn rg_and_fallback_preserve_ignore_negation_hidden_and_skip_large_target_tree() {
        let tmp = fixture();
        for result in [
            rg_search("needle", tmp.path(), &CancellationToken::new())
                .await
                .unwrap(),
            builtin_sync("needle", tmp.path()).unwrap(),
        ] {
            assert!(result.output.contains("ordinary.txt"));
            assert!(result.output.contains("keep.log"));
            for name in [
                "target/",
                "ignored.txt",
                "drop.log",
                ".hidden",
                "other.txt",
                "rg-only.txt",
            ] {
                assert!(!result.output.contains(name), "{result:?}");
            }
            assert!(result.metadata["visited"].as_u64().unwrap() < 20);
        }
    }

    #[tokio::test]
    async fn every_sensitive_rule_generates_rg_exclusions_before_unreadable_files_are_opened() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ordinary"), "needle").unwrap();
        for rule in SENSITIVE_NAMES {
            let name = rule.replace('*', "example");
            assert!(sensitive_read(Path::new(&name)), "{rule}");
            let file = tmp.path().join(&name);
            std::fs::write(&file, "needle").unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o0)).unwrap();
        }
        let mut command = rg_command_with_hidden("needle", tmp.path(), true);
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        for rule in SENSITIVE_NAMES {
            assert!(args.contains(&format!("!**/{rule}")));
            assert!(args.contains(&format!("!**/{rule}/**")));
        }
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("ordinary"));
    }

    #[tokio::test]
    async fn fallback_cancellation_size_depth_match_entry_total_byte_and_timeout_bounds() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("big"), "needle".repeat(100)).unwrap();
        let result = walk(
            "needle",
            tmp.path(),
            &CancellationToken::new(),
            Limits {
                file_bytes: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.metadata["partial"], "file size limit");
        assert!(!result.output.contains("needle"));
        std::fs::create_dir_all(tmp.path().join("a/b/c")).unwrap();
        std::fs::write(tmp.path().join("a/b/c/file"), "needle").unwrap();
        let result = walk(
            "needle",
            tmp.path(),
            &CancellationToken::new(),
            Limits {
                depth: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.metadata["partial"], "depth limit");
        let result = walk(
            "needle",
            tmp.path(),
            &CancellationToken::new(),
            Limits {
                matches: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.metadata["partial"], "match limit");
        assert_eq!(
            result
                .output
                .lines()
                .filter(|line| line.contains(":"))
                .count(),
            2
        );
        let result = walk(
            "needle",
            tmp.path(),
            &CancellationToken::new(),
            Limits {
                entries: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.metadata["partial"], "entry limit");
        let result = walk(
            "needle",
            tmp.path(),
            &CancellationToken::new(),
            Limits {
                total_bytes: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.metadata["partial"], "total byte limit");
        let result = builtin(
            "needle".into(),
            tmp.path().into(),
            CancellationToken::new(),
            Limits {
                duration: Duration::ZERO,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.metadata["partial"], "timeout");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = builtin(
            "needle".into(),
            tmp.path().into(),
            cancel,
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.status, super::super::ToolStatus::Interrupted);
    }
}
