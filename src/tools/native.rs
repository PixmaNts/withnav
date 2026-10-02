use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{
    bound_output, validate_path, Tool, ToolContext, ToolResult, DEFAULT_MAX_OUTPUT_BYTES,
};

const GREP_MAX_MATCHES: usize = 1000;

fn arg_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| ToolError::Denied {
            code: "invalid_args".into(),
            message: format!("missing string argument: {key}"),
        })
}

fn boxed<'a, F>(f: F) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>>
where
    F: Future<Output = Result<ToolResult, ToolError>> + Send + 'a,
{
    Box::pin(f)
}

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({
            "description": "Read a file or a bounded line range. Reports truncation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "1-indexed start line"},
                    "limit": {"type": "integer", "description": "max lines to read"}
                },
                "required": ["path"]
            }
        })
    }
    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        boxed(async move {
            let path = arg_str(&args, "path")?;
            let resolved = validate_path(&cx.read_roots(), Path::new(&path))?;
            if !resolved.is_file() {
                return Err(ToolError::NotFound(resolved));
            }
            let bytes = tokio::fs::read(&resolved).await?;
            let original_bytes = bytes.len();
            let text = String::from_utf8_lossy(&bytes).into_owned();

            let offset = args.get("offset").and_then(|v| v.as_u64());
            let limit = args.get("limit").and_then(|v| v.as_u64());
            let sliced = slice_lines(&text, offset, limit);

            let (out, truncated) = bound_output(&sliced, DEFAULT_MAX_OUTPUT_BYTES);
            let mut result = ToolResult::ok(out);
            if truncated.is_some() || sliced.len() < text.len() {
                result = result.with_truncation(original_bytes);
            }
            Ok(result)
        })
    }
}

fn slice_lines(text: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    if offset.is_none() && limit.is_none() {
        return text.to_string();
    }
    let start = offset.unwrap_or(1).max(1) as usize - 1;
    let lines: Vec<&str> = text.lines().collect();
    let end = match limit {
        Some(l) => (start + l as usize).min(lines.len()),
        None => lines.len(),
    };
    if start >= lines.len() {
        return String::new();
    }
    lines[start..end].join("\n")
}

pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn schema(&self) -> Value {
        json!({
            "description": "Create or replace a file atomically.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }
        })
    }
    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        boxed(async move {
            let path = arg_str(&args, "path")?;
            let content = arg_str(&args, "content")?;
            let target = Path::new(&path);

            if let Some(parent) = target.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent).await?;
                }
            }
            let resolved = validate_path(&cx.writable_roots, target)?;
            atomic_write(&resolved, content.as_bytes())?;
            Ok(ToolResult::ok(format!("wrote {} bytes", content.len())))
        })
    }
}

fn atomic_write(target: &Path, content: &[u8]) -> Result<(), ToolError> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut tmp = tempfile::NamedTempFile::new_in(&parent)?;
    tmp.write_all(content)?;
    tmp.as_file().sync_all()?;
    tmp.persist(target).map_err(|e| ToolError::Io(e.error))?;
    if let Ok(dir) = std::fs::File::open(&parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn schema(&self) -> Value {
        json!({
            "description": "Replace an exact literal string in a file. Fails on no match or ambiguous multiple matches unless replace_all.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_string", "new_string"]
            }
        })
    }
    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        boxed(async move {
            let path = arg_str(&args, "path")?;
            let old = arg_str(&args, "old_string")?;
            let new = arg_str(&args, "new_string")?;
            let replace_all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let resolved = validate_path(&cx.writable_roots, Path::new(&path))?;
            if !resolved.is_file() {
                return Err(ToolError::NotFound(resolved));
            }
            let content = tokio::fs::read_to_string(&resolved).await?;
            let count = content.matches(&old).count();
            if count == 0 {
                return Err(ToolError::NoMatch);
            }
            if count > 1 && !replace_all {
                return Err(ToolError::AmbiguousMatch { count });
            }
            let updated = if replace_all {
                content.replace(&old, &new)
            } else {
                content.replacen(&old, &new, 1)
            };
            atomic_write(&resolved, updated.as_bytes())?;
            Ok(ToolResult::ok(format!("replaced {count} occurrence(s)")))
        })
    }
}

pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn schema(&self) -> Value {
        json!({
            "description": "Search file contents for a regex pattern. Bounded output.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["pattern"]
            }
        })
    }
    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        boxed(async move {
            let pattern = arg_str(&args, "pattern")?;
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| cx.workspace.to_string_lossy().into_owned());
            let search_root = validate_path(&cx.read_roots(), Path::new(&path))?;

            if rg_available() {
                if let Some(result) = grep_rg(&pattern, &search_root).await {
                    return Ok(result);
                }
            }
            grep_builtin(&pattern, &search_root)
        })
    }
}

fn rg_available() -> bool {
    if std::env::var("NAVAI_FORCE_BUILTIN_GREP").is_ok() {
        return false;
    }
    which_rg().is_some()
}

fn which_rg() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("rg");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

async fn grep_rg(pattern: &str, root: &Path) -> Option<ToolResult> {
    let output = tokio::process::Command::new("rg")
        .arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        .arg(pattern)
        .arg(root)
        .output()
        .await
        .ok()?;
    let code = output.status.code().unwrap_or(-1);
    if code >= 2 {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let bounded = bound_matches(&stdout);
    Some(bounded.with_metadata(json!({"engine": "rg"})))
}

fn grep_builtin(pattern: &str, root: &Path) -> Result<ToolResult, ToolError> {
    let re = regex::Regex::new(pattern).map_err(|e| ToolError::Denied {
        code: "invalid_regex".into(),
        message: e.to_string(),
    })?;
    let mut out = String::new();
    let mut matches = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&p) else {
                continue;
            };
            for (lineno, line) in content.lines().enumerate() {
                if matches >= GREP_MAX_MATCHES {
                    break;
                }
                if re.is_match(line) {
                    out.push_str(&format!("{}:{}:{}\n", p.display(), lineno + 1, line));
                    matches += 1;
                }
            }
        }
    }
    Ok(bound_matches(&out).with_metadata(json!({"engine": "builtin"})))
}

fn bound_matches(s: &str) -> ToolResult {
    let (out, truncated) = bound_output(s, DEFAULT_MAX_OUTPUT_BYTES);
    let mut result = ToolResult::ok(out);
    if let Some(orig) = truncated {
        result = result.with_truncation(orig);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolStatus;

    fn ctx(root: &Path) -> ToolContext {
        ToolContext {
            workspace: root.to_path_buf(),
            writable_roots: vec![root.to_path_buf()],
            readable_roots: Vec::new(),
        }
    }

    #[tokio::test]
    async fn write_creates_file_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        let target = tmp.path().join("out.txt");
        let res = WriteTool
            .call(
                json!({"path": target.to_str().unwrap(), "content": "hello atomic"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(res.status, ToolStatus::Ok);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello atomic");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        let target = tmp.path().join("new/deep/file.txt");
        WriteTool
            .call(
                json!({"path": target.to_str().unwrap(), "content": "x"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert!(target.is_file());
    }

    #[tokio::test]
    async fn write_outside_root_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(&tmp.path().join("root"));
        std::fs::create_dir_all(&cx.workspace).unwrap();
        let target = tmp.path().join("evil.txt");
        let err = WriteTool
            .call(
                json!({"path": target.to_str().unwrap(), "content": "x"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathTraversal(_)));
    }

    #[tokio::test]
    async fn read_reports_original_bytes_on_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        let f = tmp.path().join("big.txt");
        let line = "a".repeat(99);
        let content = std::iter::repeat_n(line.as_str(), 100)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(content.len(), 10_000 - 1);
        std::fs::write(&f, &content).unwrap();
        let res = ReadTool
            .call(
                json!({"path": f.to_str().unwrap(), "offset": 1, "limit": 1}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(res.truncated.unwrap().original_bytes, content.len());
    }

    #[tokio::test]
    async fn edit_replaces_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        let f = tmp.path().join("edit.txt");
        std::fs::write(&f, "foo bar baz").unwrap();
        EditTool
            .call(
                json!({"path": f.to_str().unwrap(), "old_string": "bar", "new_string": "QUX"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "foo QUX baz");
    }

    #[tokio::test]
    async fn edit_no_match_leaves_file_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        let f = tmp.path().join("edit.txt");
        std::fs::write(&f, "original").unwrap();
        let err = EditTool
            .call(
                json!({"path": f.to_str().unwrap(), "old_string": "absent", "new_string": "X"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NoMatch));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "original");
    }

    #[tokio::test]
    async fn edit_ambiguous_match_without_replace_all_leaves_file_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        let f = tmp.path().join("edit.txt");
        std::fs::write(&f, "x x x").unwrap();
        let err = EditTool
            .call(
                json!({"path": f.to_str().unwrap(), "old_string": "x", "new_string": "Y"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::AmbiguousMatch { count: 3 }));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "x x x");
    }

    #[tokio::test]
    async fn edit_replace_all_replaces_every_match() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        let f = tmp.path().join("edit.txt");
        std::fs::write(&f, "x x x").unwrap();
        EditTool
            .call(
                json!({"path": f.to_str().unwrap(), "old_string": "x", "new_string": "Y", "replace_all": true}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "Y Y Y");
    }

    #[tokio::test]
    async fn grep_finds_known_line_with_rg() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "needle here\nother\n").unwrap();
        let res = GrepTool
            .call(
                json!({"pattern": "needle", "path": tmp.path().to_str().unwrap()}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert!(res.output.contains("needle"));
    }

    #[tokio::test]
    async fn grep_builtin_fallback_same_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "needle here\nother\n").unwrap();
        std::env::set_var("NAVAI_FORCE_BUILTIN_GREP", "1");
        let res = GrepTool
            .call(
                json!({"pattern": "needle", "path": tmp.path().to_str().unwrap()}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        std::env::remove_var("NAVAI_FORCE_BUILTIN_GREP");
        assert!(res.output.contains("needle"));
        assert_eq!(res.metadata["engine"], "builtin");
    }

    #[tokio::test]
    async fn grep_no_match_is_empty_ok_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "nothing relevant\n").unwrap();
        let res = GrepTool
            .call(
                json!({"pattern": "zzznotpresent", "path": tmp.path().to_str().unwrap()}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(res.status, ToolStatus::Ok);
        assert!(res.output.is_empty());
    }
}
