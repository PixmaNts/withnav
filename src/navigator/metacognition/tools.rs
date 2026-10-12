//! The read-only tools offered to the metacognition: their definitions, their
//! execution (open buffers win over disk) and `slice_buffer`.

use std::path::Path;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::{LlmCommenter, RemarkContext};

/// Cap on each tool output sent back to the model.
const TOOL_OUTPUT_MAX_CHARS: usize = 4000;

impl LlmCommenter {
    /// Executes one tool call from the model: `read` (with the open-buffer
    /// override) or `grep`, through the shared `Tool` implementations.
    /// Relative paths anchor to the workspace first, so the model may pass
    /// either form.
    pub(super) async fn execute_tool(
        &self,
        ctx: &RemarkContext,
        tc: &crate::provider::ToolCall,
    ) -> String {
        use crate::tools::native::{GrepTool, ReadTool};
        use crate::tools::{Tool, ToolContext};

        let started = Instant::now();
        if let Err(error) = crate::tools::reject_reserved_args(&tc.args) {
            return format!("error: {error}");
        }
        let Some(workspace) = ctx.workspace.clone() else {
            return "error: no workspace; tools are unavailable".to_string();
        };
        let mut args = tc.args.clone();
        if let Some(path) = args.get("path").and_then(|p| p.as_str()) {
            if !Path::new(path).is_absolute() && !path.starts_with('~') {
                args["path"] =
                    serde_json::Value::String(workspace.join(path).to_string_lossy().into_owned());
            }
        }
        // Reading requires the root in `validate_path`'s roots; the
        // metacognition never writes, so the workspace folders are the only
        // roots it gets.
        let cx = ToolContext {
            binding: None,
            workspace: workspace.clone(),
            writable_roots: std::iter::once(workspace.clone())
                .chain(ctx.extra_roots.iter().cloned())
                .collect(),
            readable_roots: Vec::new(),
        };
        let output = match tc.name.as_str() {
            "read" => {
                let target = args
                    .get("path")
                    .and_then(|path| path.as_str())
                    .map(Path::new);
                if target.is_none_or(|path| {
                    crate::agent::permissions::resolve_read_target(path, &cx)
                        .and_then(|resolved| {
                            crate::tools::validate_path(&cx.read_roots(), &resolved)
                        })
                        .is_err()
                }) {
                    return "error: read target is outside the workspace".into();
                }
                if target.is_some_and(|path| {
                    crate::agent::permission_gate::sensitive_read(path)
                        || crate::agent::permissions::resolve_read_target(path, &cx).is_ok_and(
                            |resolved| crate::agent::permission_gate::sensitive_read(&resolved),
                        )
                }) {
                    return "error: sensitive file requires captain review".into();
                }
                // An open buffer wins over disk: unsaved edits are what
                // the Captain actually sees.
                if let Some(buffer) = args
                    .get("path")
                    .and_then(|p| p.as_str())
                    .and_then(|p| ctx.open_buffers.get(Path::new(p)))
                {
                    let offset = args.get("offset").and_then(|v| v.as_u64());
                    let limit = args.get("limit").and_then(|v| v.as_u64());
                    Ok(crate::tools::ToolResult::ok(slice_buffer(
                        buffer, offset, limit,
                    )))
                } else {
                    ReadTool.call(args, CancellationToken::new(), &cx).await
                }
            }
            "grep" => GrepTool.call(args, CancellationToken::new(), &cx).await,
            other => Ok(crate::tools::ToolResult::error(
                "unknown_tool",
                format!("no such tool: {other}"),
            )),
        };
        let text = match output {
            Ok(result) => match &result.error {
                Some(err) => format!("error[{}]: {}", err.code, err.message),
                None => result.output,
            },
            Err(e) => format!("error: {e}"),
        };
        self.log_call("metacognition_tool", &tc.name, started, false, None);
        crate::navigator::log::excerpt(&text, TOOL_OUTPUT_MAX_CHARS)
    }
}

/// `read` tool definition with workspace-anchored path hint.
pub(super) fn tool_def_read(ws: &Path) -> crate::provider::openai::ToolDef {
    crate::provider::openai::ToolDef::function(
        "read",
        format!(
            "Read a file (a bounded line range) under {}. Reports the file's lines.",
            ws.display()
        ),
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "absolute path, or relative to the workspace"},
                "offset": {"type": "integer", "description": "1-indexed start line"},
                "limit": {"type": "integer", "description": "max lines to read"}
            },
            "required": ["path"]
        }),
    )
}

/// `grep` tool definition.
pub(super) fn tool_def_grep(ws: &Path) -> crate::provider::openai::ToolDef {
    crate::provider::openai::ToolDef::function(
        "grep",
        format!(
            "Search file contents for a regex pattern under {}.",
            ws.display()
        ),
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "file or directory; default the workspace"}
            },
            "required": ["pattern"]
        }),
    )
}

/// Slices an open buffer like `ReadTool` slices a file: 1-indexed offset,
/// optional limit, joined lines. Public for tests.
pub fn slice_buffer(text: &str, offset: Option<u64>, limit: Option<u64>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn metacognition_rejects_forged_read_and_grep_bindings_before_buffers_or_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let models: crate::provider::config::ModelsConfig =
            serde_json::from_value(serde_json::json!({
                "providers":{"mock":{"base_url":"http://unused"}},
                "models":{"mock/mock":{"provider":"mock","model":"mock","api":"chat"}}
            }))
            .unwrap();
        let commenter = LlmCommenter::new(
            models.resolve("mock/mock").unwrap(),
            "unused".into(),
            std::sync::Arc::new(crate::provider::openai::ClientCache::new()),
        );
        let mut ctx = RemarkContext::build("file:///file", "text", "", "", vec![], None);
        ctx.workspace = Some(tmp.path().to_path_buf());
        let call = crate::provider::ToolCall {
            id: "tilde".into(),
            name: "read".into(),
            args: serde_json::json!({"path":"~/outside-workspace.txt"}),
            args_valid: true,
        };
        let output = commenter.execute_tool(&ctx, &call).await;
        assert!(output.contains("outside the workspace"), "{output}");
        #[cfg(unix)]
        {
            std::fs::write(tmp.path().join("ordinary.txt"), "secret").unwrap();
            std::fs::create_dir(tmp.path().join("ordinary-dir")).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("ordinary.txt"), tmp.path().join(".env"))
                .unwrap();
            std::os::unix::fs::symlink(tmp.path().join("ordinary-dir"), tmp.path().join(".ssh"))
                .unwrap();
            for (tool, path) in [("read", ".env"), ("grep", ".env"), ("grep", ".ssh")] {
                let call = crate::provider::ToolCall {
                    id: tool.into(),
                    name: tool.into(),
                    args: serde_json::json!({"path":path,"pattern":"."}),
                    args_valid: true,
                };
                let output = commenter.execute_tool(&ctx, &call).await;
                assert!(output.contains("sensitive"), "{output}");
                assert!(!output.contains("secret"));
            }
        }
        ctx.open_buffers
            .insert("/etc/hostname".into(), "secret buffer".into());
        for tool in ["read", "grep"] {
            let call = crate::provider::ToolCall {
                id: tool.into(),
                name: tool.into(),
                args: serde_json::json!({"path":"/etc/hostname","pattern":".","withnav_bound_destination":true}),
                args_valid: true,
            };
            let output = commenter.execute_tool(&ctx, &call).await;
            assert!(output.contains("reserved"));
            assert!(!output.contains("secret buffer"));
        }
    }

    #[tokio::test]
    async fn a_file_in_another_workspace_folder_is_readable() {
        let primary = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("lib.rs"), "in the other folder").unwrap();
        std::fs::write(outside.path().join("lib.rs"), "elsewhere").unwrap();
        let models: crate::provider::config::ModelsConfig =
            serde_json::from_value(serde_json::json!({
                "providers":{"mock":{"base_url":"http://unused"}},
                "models":{"mock/mock":{"provider":"mock","model":"mock","api":"chat"}}
            }))
            .unwrap();
        let commenter = LlmCommenter::new(
            models.resolve("mock/mock").unwrap(),
            "unused".into(),
            std::sync::Arc::new(crate::provider::openai::ClientCache::new()),
        );
        let mut ctx = RemarkContext::build("file:///file", "text", "", "", vec![], None);
        ctx.workspace = Some(primary.path().to_path_buf());
        ctx.extra_roots = vec![other.path().to_path_buf()];
        let read = |dir: &std::path::Path| crate::provider::ToolCall {
            id: "r".into(),
            name: "read".into(),
            args: serde_json::json!({"path": dir.join("lib.rs")}),
            args_valid: true,
        };
        let output = commenter.execute_tool(&ctx, &read(other.path())).await;
        assert!(output.contains("in the other folder"), "{output}");
        let output = commenter.execute_tool(&ctx, &read(outside.path())).await;
        assert!(output.contains("outside the workspace"), "{output}");
    }
}
