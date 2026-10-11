use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{
    bound_output, nofollow, validate_path, Tool, ToolContext, ToolResult, DEFAULT_MAX_OUTPUT_BYTES,
};

fn arg_str(args: &Value, key: &str) -> Result<String, ToolError> {
    crate::tools::reject_reserved_args(args)?;
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
            let bound = cx
                .binding
                .as_ref()
                .map(|binding| binding.destination.clone());
            let resolved = match &bound {
                Some(path) => path.clone(),
                None => {
                    let target =
                        crate::agent::permissions::resolve_read_target(Path::new(&path), cx)?;
                    let target = validate_path(&cx.read_roots(), &target)?;
                    if crate::agent::permission_gate::sensitive_read(&target)
                        || crate::agent::permission_gate::sensitive_read(
                            &crate::agent::permissions::requested_path(Path::new(&path), cx)?,
                        )
                    {
                        return Err(ToolError::Denied {
                            code: "permission_required".into(),
                            message: "sensitive file: read requires captain review".into(),
                        });
                    }
                    target
                }
            };
            if !resolved.is_file() {
                return Err(ToolError::NotFound(resolved));
            }
            let bytes = {
                use std::io::Read;
                let mut bytes = Vec::new();
                nofollow::open_read(&resolved)?.read_to_end(&mut bytes)?;
                bytes
            };
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
    let start = usize::try_from(offset.unwrap_or(1).saturating_sub(1)).unwrap_or(usize::MAX);
    let count = usize::try_from(limit.unwrap_or(u64::MAX)).unwrap_or(usize::MAX);
    text.lines()
        .skip(start)
        .take(count)
        .collect::<Vec<_>>()
        .join("\n")
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

            // A reviewed destination is opened without following any link.
            if let Some(binding) = &cx.binding {
                let destination = &binding.destination;
                if let Some(identity) = &binding.scratch {
                    nofollow::write_scratch(destination, content.as_bytes(), identity)?;
                    return Ok(ToolResult::ok(format!("wrote {} bytes", content.len())));
                }
                nofollow::write_atomic(destination, content.as_bytes())?;
                return Ok(ToolResult::ok(format!("wrote {} bytes", content.len())));
            }

            // Validate first: a refused path must leave nothing on disk, not
            // even the folders leading to it.
            let resolved = validate_path(
                &cx.writable_roots,
                &crate::agent::permissions::resolve_read_target(Path::new(&path), cx)?,
            )?;
            if let Some(parent) = resolved.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
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
            // An empty needle matches at every character boundary: it would
            // either report a bogus ambiguity or interleave `new_string`
            // through the whole file under `replace_all`.
            if old.is_empty() {
                return Err(ToolError::Denied {
                    code: "invalid_args".into(),
                    message: "old_string cannot be empty".into(),
                });
            }
            let replace_all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let change = |content: String| {
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
                Ok((updated, format!("replaced {count} occurrence(s)")))
            };
            // A reviewed destination is read and written back without
            // following any link.
            if let Some(binding) = &cx.binding {
                let destination = &binding.destination;
                let report = if let Some(identity) = &binding.scratch {
                    nofollow::edit_scratch(destination, identity, change)?
                } else {
                    nofollow::edit_atomic(destination, change)?
                };
                return Ok(ToolResult::ok(report));
            }

            let resolved = validate_path(
                &cx.writable_roots,
                &crate::agent::permissions::resolve_read_target(Path::new(&path), cx)?,
            )?;
            if !resolved.is_file() {
                return Err(ToolError::NotFound(resolved));
            }
            let content = tokio::fs::read_to_string(&resolved).await?;
            let (updated, report) = change(content)?;
            atomic_write(&resolved, updated.as_bytes())?;
            Ok(ToolResult::ok(report))
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
            let search_root = cx
                .binding
                .as_ref()
                .map(|binding| Ok(binding.destination.clone()))
                .unwrap_or_else(|| {
                    crate::agent::permissions::resolve_read_target(Path::new(&path), cx)
                })?;
            let search_root =
                validate_path(&cx.read_roots(), &search_root).map_err(|_| ToolError::Denied {
                    code: "outside_project".into(),
                    message: crate::tools::OUTSIDE_SEARCH.into(),
                })?;
            if crate::agent::permission_gate::sensitive_read(
                &crate::agent::permissions::requested_path(Path::new(&path), cx)?,
            ) || crate::agent::permission_gate::sensitive_read(&search_root)
            {
                return Err(ToolError::Denied {
                    code: "sensitive_path".into(),
                    message:
                        "sensitive search root: use read on individual files; each read is reviewed"
                            .into(),
                });
            }
            super::grep::search(pattern, search_root, _cancel).await
        })
    }
}

#[cfg(test)]
use super::grep::rg_command;
#[cfg(test)]
fn grep_builtin(pattern: &str, root: &Path) -> Result<ToolResult, ToolError> {
    super::grep::builtin_sync(pattern, root)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn unbound_relative_reads_preserve_sensitive_workspace_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let ordinary = tmp.path().join("ordinary");
        std::fs::create_dir(&ordinary).unwrap();
        std::fs::write(ordinary.join("config"), "secret").unwrap();
        let workspace = tmp.path().join(".ssh");
        std::os::unix::fs::symlink(ordinary, &workspace).unwrap();
        let cx = ctx(&workspace);
        for (tool, path) in [
            (&ReadTool as &dyn Tool, "config"),
            (&GrepTool as &dyn Tool, "."),
        ] {
            let error = tool
                .call(
                    json!({"path":path,"pattern":"."}),
                    CancellationToken::new(),
                    &cx,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("sensitive"));
        }
    }
    #[tokio::test]
    async fn unbound_native_read_and_search_reject_sensitive_alias_spellings() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ordinary.txt"), "secret").unwrap();
        std::fs::create_dir(tmp.path().join("ordinary-dir")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("ordinary.txt"), tmp.path().join(".env"))
            .unwrap();
        std::os::unix::fs::symlink(tmp.path().join("ordinary-dir"), tmp.path().join(".ssh"))
            .unwrap();
        let cx = ctx(tmp.path());
        for (tool, path) in [
            (&ReadTool as &dyn Tool, ".env"),
            (&GrepTool as &dyn Tool, ".env"),
            (&GrepTool as &dyn Tool, ".ssh"),
            (&crate::tools::glob::GlobTool as &dyn Tool, ".ssh"),
        ] {
            let error = tool
                .call(
                    json!({"path":path,"pattern":"."}),
                    CancellationToken::new(),
                    &cx,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("sensitive"));
        }
    }
    #[tokio::test]
    async fn grep_skips_sensitive_files_before_opening_with_rg_available_or_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("config.json"), "ordinary credentials word").unwrap();
        for name in [
            "auth.json",
            "credentials.json",
            "token.key",
            ".env",
            ".env.example",
        ] {
            std::fs::write(root.join(name), "TOPSECRET").unwrap();
        }
        std::fs::create_dir(root.join(".ssh")).unwrap();
        std::fs::write(root.join(".ssh/id_ed25519"), "TOPSECRET").unwrap();
        let cx = ctx(root);
        let result = GrepTool
            .call(
                json!({"path":root,"pattern":"."}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert!(result.output.contains("ordinary credentials word"));
        assert!(!result.output.contains("TOPSECRET"));
        assert!(
            result.output.contains("sensitive files skipped"),
            "{result:?}"
        );
        let builtin = grep_builtin(".", root).unwrap();
        assert!(!builtin.output.contains("TOPSECRET"));
        assert_eq!(result.metadata["engine"], "rg");
    }

    #[tokio::test]
    async fn direct_native_ingress_rejects_forged_read_and_grep_bindings() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        for tool in [&ReadTool as &dyn Tool, &GrepTool as &dyn Tool] {
            let error = tool
                .call(
                    json!({"path":"/etc/hostname","pattern":".","withnav_bound_destination":true}),
                    CancellationToken::new(),
                    &cx,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("reserved"));
        }
    }
    #[test]
    fn rg_option_looking_patterns_are_data_and_config_is_disabled() {
        for pattern in ["--pre=/some/program", "--glob=../*", "-e", "normal"] {
            let command = super::rg_command(pattern, std::path::Path::new("/validated/root"));
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            assert_eq!(
                &args[..4],
                &[
                    "--no-config",
                    "--line-number",
                    "--no-heading",
                    "--color=never"
                ]
            );
            assert_eq!(
                &args[args.len() - 4..],
                &["--regexp", pattern, "--", "/validated/root"]
            );
        }
    }
    use super::*;
    use crate::tools::ToolStatus;

    fn ctx(root: &Path) -> ToolContext {
        ToolContext {
            binding: None,
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
    async fn a_rejected_write_creates_no_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let cx = ctx(&root);
        let target = tmp.path().join("outside/deep/file.txt");
        let res = WriteTool
            .call(
                json!({"path": target.to_str().unwrap(), "content": "x"}),
                CancellationToken::new(),
                &cx,
            )
            .await;
        assert!(res.is_err());
        assert!(!tmp.path().join("outside").exists());
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
        for (offset, limit, expected) in [
            (0, 1, line.as_str()),
            (2, u64::MAX, &content[100..]),
            (u64::MAX, u64::MAX, ""),
            (1, 0, ""),
        ] {
            assert_eq!(slice_lines(&content, Some(offset), Some(limit)), expected);
        }
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
    async fn edit_rejects_empty_old_string_and_leaves_file_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = ctx(tmp.path());
        let f = tmp.path().join("edit.txt");
        std::fs::write(&f, "abc").unwrap();
        let err = EditTool
            .call(
                json!({"path": f.to_str().unwrap(), "old_string": "", "new_string": "#", "replace_all": true}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { ref code, .. } if code == "invalid_args"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "abc");
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
        std::fs::write(tmp.path().join("a.txt"), "needle here\nother\n").unwrap();
        let res = super::super::grep::search_with(
            super::super::grep::Engine::Builtin,
            "needle".into(),
            tmp.path().into(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(res.output.contains("needle"));
        assert_eq!(res.metadata["engine"], "builtin");
    }

    // Given a readable root holding a link to a file and a directory outside
    //       it, and a link back to an ancestor
    // When the built-in search runs
    // Then only the real files are searched and the search ends
    #[test]
    fn grep_builtin_does_not_follow_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(outside.join("dir")).unwrap();
        std::fs::write(root.join("sub/real.txt"), "needle real\n").unwrap();
        std::fs::write(outside.join("secret.txt"), "needle secret\n").unwrap();
        std::fs::write(outside.join("dir/deep.txt"), "needle deep\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("file-link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("dir-link")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("sub/back")).unwrap();

        let res = grep_builtin("needle", &root).unwrap();
        assert!(res.output.contains("needle real"), "{}", res.output);
        assert!(!res.output.contains("secret"), "{}", res.output);
        assert!(!res.output.contains("deep"), "{}", res.output);
        assert_eq!(res.output.lines().count(), 1, "{}", res.output);
    }

    // Given the tool with the built-in engine forced, a readable root with an
    //       escaping link, and a link cycle
    // When it searches the root
    // Then it ends without reading what the links lead to
    #[tokio::test]
    async fn grep_tool_with_the_builtin_engine_ignores_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("a.txt"), "needle inside\n").unwrap();
        std::fs::write(outside.join("b.txt"), "needle outside\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
        let res = super::super::grep::search_with(
            super::super::grep::Engine::Builtin,
            "needle".into(),
            root,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(res.output.contains("needle inside"), "{}", res.output);
        assert!(!res.output.contains("outside"), "{}", res.output);
    }

    // Given a search root that is itself a link to a directory
    // When the built-in search runs on the validated, canonical root
    // Then its files are searched
    #[tokio::test]
    async fn grep_builtin_searches_a_linked_root_through_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("a.txt"), "needle\n").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let cx = ctx(tmp.path());
        let resolved = validate_path(&cx.read_roots(), &link).unwrap();
        assert!(grep_builtin("needle", &resolved)
            .unwrap()
            .output
            .contains("needle"));
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
