//! `glob`: find files by a relative pattern under a validated root.
//!
//! The walk is the one `grep` uses without `rg`: plain `read_dir`, never
//! following a link, so a link out of a readable root or back to an ancestor
//! cannot widen or loop it. It is bounded in entries visited and in time.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

use globset::{Glob, GlobBuilder};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{
    bound_output, validate_path, Tool, ToolContext, ToolResult, DEFAULT_MAX_OUTPUT_BYTES,
};

pub const GLOB_DEFAULT_LIMIT: usize = 200;
pub const GLOB_MAX_LIMIT: usize = 1_000;
pub const GLOB_MAX_VISITED: usize = 100_000;
pub const GLOB_MAX_DURATION: Duration = Duration::from_secs(5);

/// Directories skipped like hidden ones, unless the pattern names them.
const SKIPPED_DIRS: [&str; 2] = ["node_modules", "target"];

pub struct GlobTool;

fn denied(code: &str, message: impl Into<String>) -> ToolError {
    ToolError::Denied {
        code: code.into(),
        message: message.into(),
    }
}

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn schema(&self) -> Value {
        json!({
            "description": "Find files by glob pattern (`**/*.rs`, `src/**/mod.rs`). The pattern is relative to the search root, which is the workspace unless `path` is given. Returns sorted relative file paths. Links are not followed; hidden entries, `node_modules` and `target` are skipped unless the pattern names them (`.github/**`, `node_modules/foo/*.json`). Bounded: at most `limit` paths (default 200, max 1000), 100,000 entries visited, 5 seconds.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "relative glob pattern; no absolute path, no `..`"},
                    "path": {"type": "string", "description": "directory to search (default: the workspace)"},
                    "limit": {"type": "integer", "description": "max paths to return (default 200, max 1000)"}
                },
                "required": ["pattern"]
            }
        })
    }

    fn call<'a>(
        &'a self,
        args: Value,
        cancel: CancellationToken,
        cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            crate::tools::reject_reserved_args(&args)?;
            let pattern = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| denied("invalid_args", "missing string argument: pattern"))?
                .to_string();
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|l| (l as usize).clamp(1, GLOB_MAX_LIMIT))
                .unwrap_or(GLOB_DEFAULT_LIMIT);

            let matcher = Matcher::new(&pattern)?;

            let requested = match args.get("path").and_then(|v| v.as_str()) {
                Some(p) => {
                    let p = Path::new(p);
                    if p.is_absolute() {
                        p.to_path_buf()
                    } else {
                        cx.workspace.join(p)
                    }
                }
                None => cx.workspace.clone(),
            };
            if crate::agent::permission_gate::sensitive_read(&requested) {
                return Err(denied(
                    "sensitive_path",
                    "sensitive search root: use read on individual files; each read is reviewed",
                ));
            }
            let requested = cx
                .binding
                .as_ref()
                .map(|binding| binding.destination.clone())
                .unwrap_or(requested);
            let roots = cx.read_roots();
            let canonical = validate_path(&roots, &requested)
                .map_err(|_| denied("outside_project", crate::tools::OUTSIDE_SEARCH))?;
            if crate::agent::permission_gate::sensitive_read(&canonical) {
                return Err(denied(
                    "sensitive_path",
                    "sensitive search root: use read on individual files; each read is reviewed",
                ));
            }
            search(
                roots,
                requested,
                matcher,
                limit,
                cancel,
                GLOB_MAX_DURATION + GLOB_TIMEOUT_MARGIN,
                || {},
            )
            .await
        })
    }
}

/// Slack past the walk's own deadline before the caller stops waiting for the
/// worker: a single slow filesystem call cannot be interrupted, only
/// abandoned.
const GLOB_TIMEOUT_MARGIN: Duration = Duration::from_millis(500);

/// Validation, walk and rendering on a blocking worker, under an outer
/// timeout. On expiry the worker's token is cancelled (it stops at its next
/// check) and the result says the search stopped on its time limit.
/// `before_walk` runs on the worker after validation; tests use it to stall.
async fn search(
    roots: Vec<PathBuf>,
    requested: PathBuf,
    matcher: Matcher,
    limit: usize,
    cancel: CancellationToken,
    outer: Duration,
    before_walk: impl FnOnce() + Send + 'static,
) -> Result<ToolResult, ToolError> {
    let worker_cancel = cancel.child_token();
    let token = worker_cancel.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let root = validate_path(&roots, &requested)?;
        if !root.is_dir() {
            return Err(ToolError::NotFound(root));
        }
        before_walk();
        let walked = walk(&root, &matcher, &token, GLOB_MAX_VISITED, GLOB_MAX_DURATION);
        if walked.stop == Stop::Canceled {
            return Ok(ToolResult::interrupted());
        }
        Ok(render(walked, limit))
    });
    tokio::select! {
        _ = cancel.cancelled() => {
            worker_cancel.cancel();
            Ok(ToolResult::interrupted())
        }
        timed = tokio::time::timeout(outer, worker) => match timed {
            Ok(joined) => joined.map_err(|e| ToolError::Io(std::io::Error::other(e)))?,
            Err(_) => {
                worker_cancel.cancel();
                Ok(render(
                    Walked { paths: Vec::new(), visited: 0, sensitive_skipped: 0, stop: Stop::Timeout },
                    limit,
                ))
            }
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Done,
    Canceled,
    VisitedLimit,
    Timeout,
}

struct Walked {
    sensitive_skipped: usize,
    paths: Vec<String>,
    visited: usize,
    stop: Stop,
}

struct Matcher {
    glob: globset::GlobMatcher,
    /// Pattern segments that name an entry explicitly (they do not start with
    /// a wildcard), as matchers for hidden or skipped names.
    named: Vec<globset::GlobMatcher>,
}

impl Matcher {
    fn new(pattern: &str) -> Result<Self, ToolError> {
        if pattern.trim().is_empty() {
            return Err(denied("invalid_pattern", "the pattern is empty"));
        }
        if Path::new(pattern).is_absolute() || pattern.starts_with('~') {
            return Err(denied(
                "invalid_pattern",
                "the pattern must be relative; pass the directory as `path`",
            ));
        }
        if pattern.split('/').any(|seg| seg == "..") {
            return Err(denied(
                "invalid_pattern",
                "the pattern must not contain `..`; pass the directory as `path`",
            ));
        }
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| denied("invalid_pattern", e.to_string()))?
            .compile_matcher();
        let named = pattern
            .split('/')
            .filter(|seg| {
                !seg.is_empty()
                    && *seg != "**"
                    && (!seg.starts_with(is_wildcard)
                        || (seg.starts_with('{')
                            && seg.ends_with('}')
                            && seg[1..seg.len() - 1].split(',').all(|alternative| {
                                !alternative.is_empty() && !alternative.starts_with(is_wildcard)
                            })))
            })
            .filter_map(|seg| Glob::new(seg).ok())
            .map(|g| g.compile_matcher())
            .collect();
        Ok(Self { glob, named })
    }

    /// Hidden entries and the skipped directories are walked only when the
    /// pattern names them.
    fn admits(&self, name: &str) -> bool {
        let default_skipped = name.starts_with('.') || SKIPPED_DIRS.contains(&name);
        !default_skipped || self.named.iter().any(|m| m.is_match(name))
    }
}

fn is_wildcard(c: char) -> bool {
    matches!(c, '*' | '?' | '[' | ']' | '{' | '}')
}

fn walk(
    root: &Path,
    matcher: &Matcher,
    cancel: &CancellationToken,
    max_visited: usize,
    max_duration: Duration,
) -> Walked {
    let started = Instant::now();
    let mut out = Walked {
        sensitive_skipped: 0,
        paths: Vec::new(),
        visited: 0,
        stop: Stop::Done,
    };
    if crate::agent::permission_gate::sensitive_read(root) {
        out.sensitive_skipped = 1;
        return out;
    }
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            out.stop = Stop::Canceled;
            return out;
        }
        if started.elapsed() >= max_duration {
            out.stop = Stop::Timeout;
            return out;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if cancel.is_cancelled() {
                out.stop = Stop::Canceled;
                return out;
            }
            if started.elapsed() >= max_duration {
                out.stop = Stop::Timeout;
                return out;
            }
            if out.visited >= max_visited {
                out.stop = Stop::VisitedLimit;
                return out;
            }
            out.visited += 1;
            // The kind of the entry itself: a link is neither listed nor
            // followed.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() && !kind.is_file() {
                continue;
            }
            let name = entry.file_name();
            let path = entry.path();
            if crate::agent::permission_gate::sensitive_read(&path) {
                out.sensitive_skipped += 1;
                continue;
            }
            let Some(name) = name.to_str() else {
                continue;
            };
            if !matcher.admits(name) {
                continue;
            }
            if kind.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root) {
                if matcher.glob.is_match(rel) {
                    out.paths.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    out
}

fn render(mut walked: Walked, limit: usize) -> ToolResult {
    walked.paths.sort();
    let total = walked.paths.len();
    let cut = total > limit;
    walked.paths.truncate(limit);

    let stopped = match walked.stop {
        Stop::VisitedLimit => Some("visited_limit"),
        Stop::Timeout => Some("timeout"),
        _ => None,
    };
    let mut text = walked.paths.join("\n");
    if walked.sensitive_skipped > 0 {
        text.push_str(&format!(
            "\n{} sensitive files skipped; read them individually (reviewed)",
            walked.sensitive_skipped
        ));
    }
    if cut || stopped.is_some() {
        let mut note = String::from("[");
        if cut {
            note.push_str(&format!("showing {limit} of {total} matches"));
        }
        match stopped {
            Some("visited_limit") => {
                if cut {
                    note.push_str("; ");
                }
                note.push_str(&format!(
                    "search stopped after {} entries, more may exist",
                    walked.visited
                ));
            }
            Some(_) => {
                if cut {
                    note.push_str("; ");
                }
                note.push_str("search stopped on its time limit, more may exist");
            }
            None => {}
        }
        note.push(']');
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&note);
    }

    let (bounded, elided) = bound_output(&text, DEFAULT_MAX_OUTPUT_BYTES);
    let mut result = ToolResult::ok(bounded).with_metadata(json!({
        "total_matches": total,
        "returned": limit.min(total),
        "truncated": cut || stopped.is_some(),
        "stopped": stopped,
        "visited": walked.visited,
    }));
    if let Some(original) = elided {
        result = result.with_truncation(original);
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn brace_alternatives_admit_explicit_skipped_directories() {
        let matcher = super::Matcher::new("{src,target}/**/*.rs").unwrap();
        assert!(matcher.admits("target"));
        assert!(!matcher.admits("node_modules"));
        let hidden = super::Matcher::new("{src,.github}/**/*").unwrap();
        assert!(hidden.admits(".github"));
        let broad = super::Matcher::new("{src,*}/**/*").unwrap();
        assert!(!broad.admits("target"));
    }
    #[tokio::test]
    async fn glob_skips_sensitive_files_even_when_pattern_explicitly_names_them() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "config.json");
        for name in [
            "auth.json",
            "credentials.json",
            "token.key",
            ".env.sample",
            ".ssh/id_ed25519",
        ] {
            touch(tmp.path(), name);
        }
        let result = run(&cx(tmp.path()), json!({"pattern":"**/*"}))
            .await
            .unwrap();
        assert!(result.output.contains("config.json"));
        for name in ["auth.json", "credentials.json", "token.key"] {
            assert!(!result.output.contains(name));
        }
        let result = run(&cx(tmp.path()), json!({"pattern":".ssh/**"}))
            .await
            .unwrap();
        assert!(result.output.contains("sensitive files skipped"));
        assert!(!result.output.contains("id_ed25519"));
    }
    use super::*;
    use crate::tools::ToolStatus;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "x").unwrap();
    }

    fn cx(root: &Path) -> ToolContext {
        ToolContext {
            binding: None,
            workspace: root.to_path_buf(),
            writable_roots: vec![root.to_path_buf()],
            readable_roots: Vec::new(),
        }
    }

    async fn run(cx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        GlobTool.call(args, CancellationToken::new(), cx).await
    }

    fn lines(res: &ToolResult) -> Vec<&str> {
        res.output
            .lines()
            .filter(|line| !line.contains("sensitive files skipped") && !line.is_empty())
            .collect()
    }

    // Given a tree of sources and docs
    // When a recursive pattern and a top-level pattern run
    // Then the first finds nested files, the second only top-level ones, both sorted
    #[tokio::test]
    async fn patterns_match_relative_sorted_paths() {
        let tmp = tempfile::tempdir().unwrap();
        for f in ["b.rs", "a.rs", "src/z.rs", "src/deep/y.rs", "README.md"] {
            touch(tmp.path(), f);
        }
        let cx = cx(tmp.path());
        let all = run(&cx, json!({"pattern": "**/*.rs"})).await.unwrap();
        assert_eq!(
            lines(&all),
            ["a.rs", "b.rs", "src/deep/y.rs", "src/z.rs"],
            "{}",
            all.output
        );
        let top = run(&cx, json!({"pattern": "*.rs"})).await.unwrap();
        assert_eq!(lines(&top), ["a.rs", "b.rs"]);
        let sub = run(&cx, json!({"pattern": "src/**/*.rs", "path": "."}))
            .await
            .unwrap();
        assert_eq!(lines(&sub), ["src/deep/y.rs", "src/z.rs"]);
        assert_eq!(all.metadata["truncated"], false);
    }

    // Given a subdirectory as `path`
    // When the pattern runs
    // Then the paths are relative to that directory
    #[tokio::test]
    async fn path_scopes_the_search_root() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "src/a.rs");
        touch(tmp.path(), "other/b.rs");
        let res = run(&cx(tmp.path()), json!({"pattern": "*.rs", "path": "src"}))
            .await
            .unwrap();
        assert_eq!(lines(&res), ["a.rs"]);
    }

    // Given hidden, VCS, node_modules and target directories
    // When a broad pattern runs, then patterns that name them
    // Then the first skips them and the others reach them
    #[tokio::test]
    async fn hidden_and_vcs_dirs_are_skipped_unless_named() {
        let tmp = tempfile::tempdir().unwrap();
        for f in [
            "src/a.txt",
            ".git/config.txt",
            ".github/ci.txt",
            ".env.txt",
            "node_modules/pkg/i.txt",
            "target/debug/t.txt",
        ] {
            touch(tmp.path(), f);
        }
        let cx = cx(tmp.path());
        let broad = run(&cx, json!({"pattern": "**/*.txt"})).await.unwrap();
        assert_eq!(lines(&broad), ["src/a.txt"], "{}", broad.output);

        let gh = run(&cx, json!({"pattern": ".github/**"})).await.unwrap();
        assert_eq!(lines(&gh), [".github/ci.txt"]);
        let git = run(&cx, json!({"pattern": "**/.git/*.txt"})).await.unwrap();
        assert_eq!(lines(&git), [".git/config.txt"]);
        let nm = run(&cx, json!({"pattern": "node_modules/**/*.txt"}))
            .await
            .unwrap();
        assert_eq!(lines(&nm), ["node_modules/pkg/i.txt"]);
        let tg = run(&cx, json!({"pattern": "target/**/*.txt"}))
            .await
            .unwrap();
        assert_eq!(lines(&tg), ["target/debug/t.txt"]);
        let dot = run(&cx, json!({"pattern": ".*.txt"})).await.unwrap();
        assert!(!dot.output.contains(".env.txt"));
        assert!(dot.output.contains("1 sensitive files skipped"));
    }

    // Given links to a file, a directory outside, and back to the root
    // When the walk runs
    // Then none is listed or followed
    #[tokio::test]
    async fn links_are_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        touch(&root, "real.txt");
        touch(&outside, "secret.txt");
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("file-link.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("dir-link")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
        let res = run(&cx(&root), json!({"pattern": "**/*.txt"}))
            .await
            .unwrap();
        assert_eq!(lines(&res), ["real.txt"], "{}", res.output);
    }

    // Given absolute, `..` and malformed patterns
    // When the tool runs
    // Then each is refused with invalid_pattern
    #[tokio::test]
    async fn absolute_and_escaping_patterns_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = cx(tmp.path());
        for bad in ["/etc/*", "../*", "src/../../*", "~/x", "", "[a"] {
            let err = run(&cx, json!({"pattern": bad})).await.unwrap_err();
            match err {
                ToolError::Denied { code, .. } => assert_eq!(code, "invalid_pattern", "{bad}"),
                other => panic!("{bad}: {other:?}"),
            }
        }
        let err = run(&cx, json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::Denied { code, .. } if code == "invalid_args"));
    }

    // Given a `path` outside the roots, and one that is a file
    // When the tool runs
    // Then they are refused
    #[tokio::test]
    async fn path_outside_the_roots_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        touch(&root, "a.txt");
        touch(tmp.path(), "outside/b.txt");
        let cx = cx(&root);
        let err = run(
            &cx,
            json!({"pattern": "*", "path": tmp.path().join("outside").to_str().unwrap()}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains(crate::tools::OUTSIDE_SEARCH));
        let err = run(&cx, json!({"pattern": "*", "path": "../outside"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains(crate::tools::OUTSIDE_SEARCH));
        let err = run(&cx, json!({"pattern": "*", "path": "a.txt"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    // Given a readable (read-only) root outside the workspace
    // When `path` names it
    // Then it is searched
    #[tokio::test]
    async fn readable_roots_are_searchable() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let skills = tmp.path().join("skills");
        touch(&ws, "a.txt");
        touch(&skills, "s/SKILL.md");
        let cx = ToolContext {
            binding: None,
            workspace: ws.clone(),
            writable_roots: vec![ws],
            readable_roots: vec![skills.clone()],
        };
        let res = run(
            &cx,
            json!({"pattern": "**/SKILL.md", "path": skills.to_str().unwrap()}),
        )
        .await
        .unwrap();
        assert_eq!(lines(&res), ["s/SKILL.md"]);
    }

    // Given more matches than the limit
    // When the tool runs with a small, a default and an oversized limit
    // Then the result is cut with a visible note and metadata
    #[tokio::test]
    async fn limit_truncates_with_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..1_100 {
            touch(tmp.path(), &format!("f{i:04}.txt"));
        }
        let cx = cx(tmp.path());
        let small = run(&cx, json!({"pattern": "*.txt", "limit": 3}))
            .await
            .unwrap();
        assert_eq!(small.status, ToolStatus::Ok);
        assert_eq!(
            lines(&small),
            [
                "f0000.txt",
                "f0001.txt",
                "f0002.txt",
                "[showing 3 of 1100 matches]"
            ]
        );
        assert_eq!(small.metadata["total_matches"], 1100);
        assert_eq!(small.metadata["truncated"], true);

        let default = run(&cx, json!({"pattern": "*.txt"})).await.unwrap();
        assert_eq!(default.metadata["returned"], GLOB_DEFAULT_LIMIT);
        let huge = run(&cx, json!({"pattern": "*.txt", "limit": 50_000}))
            .await
            .unwrap();
        assert_eq!(huge.metadata["returned"], GLOB_MAX_LIMIT);
    }

    // Given a tree larger than the visit bound, and one past an expired deadline
    // When the walk runs
    // Then it stops early and says why
    #[test]
    fn walk_stops_on_the_visit_and_time_bounds() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..50 {
            touch(tmp.path(), &format!("d/f{i}.txt"));
        }
        let m = Matcher::new("**/*.txt").unwrap();
        let cancel = CancellationToken::new();
        let by_count = walk(tmp.path(), &m, &cancel, 10, Duration::from_secs(60));
        assert_eq!(by_count.stop, Stop::VisitedLimit);
        assert_eq!(by_count.visited, 10);
        let by_time = walk(tmp.path(), &m, &cancel, 1_000, Duration::ZERO);
        assert_eq!(by_time.stop, Stop::Timeout);
        let rendered = render(by_count, 200);
        assert_eq!(rendered.metadata["stopped"], "visited_limit");
        assert!(rendered.output.contains("search stopped after 10 entries"));
    }

    // Given many empty directories and a cancelled token or an expired deadline
    // When the walk runs
    // Then it reports cancellation or timeout though nothing matched
    #[test]
    fn empty_directories_still_honour_cancellation_and_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..200 {
            std::fs::create_dir(tmp.path().join(format!("d{i}"))).unwrap();
        }
        let m = Matcher::new("**/*.txt").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let walked = walk(tmp.path(), &m, &cancel, 1_000, Duration::from_secs(60));
        assert_eq!(walked.stop, Stop::Canceled);
        assert!(walked.paths.is_empty());

        let walked = walk(
            tmp.path(),
            &m,
            &CancellationToken::new(),
            1_000,
            Duration::ZERO,
        );
        assert_eq!(walked.stop, Stop::Timeout);
        assert!(walked.visited < 200);
    }

    // Given a worker that stalls past the outer timeout
    // When the search runs
    // Then it answers "search stopped" on time and the worker's token is cancelled
    #[tokio::test]
    async fn a_stalled_worker_is_abandoned_at_the_outer_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.txt");
        let cx = cx(tmp.path());
        let started = Instant::now();
        let res = search(
            cx.read_roots(),
            tmp.path().to_path_buf(),
            Matcher::new("*.txt").unwrap(),
            10,
            CancellationToken::new(),
            Duration::from_millis(100),
            || std::thread::sleep(Duration::from_millis(1500)),
        )
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(1000));
        assert_eq!(res.status, ToolStatus::Ok);
        assert_eq!(res.metadata["stopped"], "timeout");
        assert!(res.output.contains("search stopped on its time limit"));
    }

    // Given a cancelled token and only empty directories
    // When the tool runs
    // Then the result is interrupted
    #[tokio::test]
    async fn cancellation_interrupts_even_when_nothing_matched() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::create_dir(tmp.path().join(format!("d{i}"))).unwrap();
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        let res = GlobTool
            .call(json!({"pattern": "**/*.txt"}), cancel, &cx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(res.status, ToolStatus::Interrupted);
    }

    // Given a cancelled token
    // When the tool runs
    // Then the result is interrupted
    #[tokio::test]
    async fn cancellation_interrupts_the_walk() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.txt");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let res = GlobTool
            .call(json!({"pattern": "**/*"}), cancel, &cx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(res.status, ToolStatus::Interrupted);
    }
}
