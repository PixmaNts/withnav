//! The cognition's `reveal` tool: asks the Captain's editor to open a
//! workspace file and put the cursor on a line or a symbol. The server only
//! validates the target; the editor extension acts on the successful
//! `tool.post_call` (its `metadata.reveal`), and resolves the symbol itself.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{Tool, ToolContext, ToolResult, validate_path};

/// Registered only in editor sessions, like `intent`.
pub struct RevealTool;

impl Tool for RevealTool {
    fn name(&self) -> &str {
        "reveal"
    }

    fn schema(&self) -> Value {
        json!({
            "description": "Open a workspace file in the Captain's editor and put the cursor, \
                centered, on a symbol or a line. Use it when you point the Captain to a place \
                in the code (\"in gstreamer.c, go to gstreamer_send_frame()\"), once per answer, \
                on the place the Captain should look at first.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace"},
                    "symbol": {"type": "string", "description": "Function, type or variable name to put the cursor on"},
                    "line": {"type": "integer", "minimum": 1, "description": "1-based line; with a symbol, picks the occurrence nearest to it"}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        })
    }

    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(path) = args["path"].as_str().filter(|p| !p.trim().is_empty()) else {
                return Ok(ToolResult::error("invalid_args", "path is required"));
            };
            let line = match &args["line"] {
                Value::Null => None,
                value => match value.as_u64().filter(|n| *n >= 1) {
                    Some(n) => Some(n),
                    None => {
                        return Ok(ToolResult::error(
                            "invalid_args",
                            "line must be an integer >= 1",
                        ));
                    }
                },
            };
            let symbol = args["symbol"]
                .as_str()
                .map(|s| s.trim().trim_end_matches("()").trim())
                .filter(|s| !s.is_empty());
            // The editor only opens files of its workspace: the writable roots.
            let resolved = validate_path(&cx.writable_roots, Path::new(path))?;
            if !resolved.is_file() {
                return Ok(ToolResult::error(
                    "not_found",
                    format!("no such file: {path}"),
                ));
            }
            let mut target = json!({ "path": resolved.to_string_lossy() });
            if let Some(line) = line {
                target["line"] = json!(line);
            }
            if let Some(symbol) = symbol {
                target["symbol"] = json!(symbol);
            }
            Ok(ToolResult::ok(format!("revealed in the editor: {path}"))
                .with_metadata(json!({ "reveal": target })))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx(root: &Path) -> ToolContext {
        ToolContext {
            binding: None,
            workspace: root.to_path_buf(),
            writable_roots: vec![root.to_path_buf()],
            readable_roots: Vec::new(),
        }
    }

    #[tokio::test]
    async fn reveal_validates_and_carries_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("gstreamer.c"), "int x;\n").unwrap();
        let cx = cx(tmp.path());
        let ok = RevealTool
            .call(
                json!({"path": "gstreamer.c", "symbol": "gstreamer_send_frame()", "line": 3}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(ok.status, crate::tools::ToolStatus::Ok, "{ok:?}");
        assert_eq!(ok.metadata["reveal"]["symbol"], "gstreamer_send_frame");
        assert_eq!(ok.metadata["reveal"]["line"], 3);

        let missing = RevealTool
            .call(json!({"path": "nope.c"}), CancellationToken::new(), &cx)
            .await
            .unwrap();
        assert_eq!(missing.error.unwrap().code, "not_found");

        let bad_line = RevealTool
            .call(
                json!({"path": "gstreamer.c", "line": 0}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(bad_line.error.unwrap().code, "invalid_args");

        assert!(
            RevealTool
                .call(
                    json!({"path": "../outside.c"}),
                    CancellationToken::new(),
                    &cx
                )
                .await
                .is_err()
        );
    }
}
