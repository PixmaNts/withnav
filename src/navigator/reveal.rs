//! The cognition's `reveal` tool: opens a workspace file in the Captain's
//! editor and puts the cursor on a line or a symbol, in the active editor
//! group or beside it. The server validates the target, then asks the editor
//! over the language server (`withnav/reveal`) and reports what it did.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::navigator::conversation::ConversationFeed;
use crate::navigator::lsp::protocol::{RevealParams, RevealPlacement};
use crate::tools::{Tool, ToolContext, ToolResult, validate_path};

/// How long the editor has to open the file and answer.
const EDITOR_TIMEOUT: Duration = Duration::from_secs(10);

/// Registered only in editor sessions, like `intent`.
pub struct RevealTool {
    feed: Option<ConversationFeed>,
}

impl RevealTool {
    /// `feed` carries the connected editor; without one every call fails.
    pub fn new(feed: Option<ConversationFeed>) -> Self {
        Self { feed }
    }
}

impl Tool for RevealTool {
    fn name(&self) -> &str {
        "reveal"
    }

    fn schema(&self) -> Value {
        json!({
            "description": "Open a workspace file in the Captain's editor and put the cursor, \
                centered, on a symbol or a line. Use it when you point the Captain to a place \
                in the code (\"in gstreamer.c, go to gstreamer_send_frame()\"), once per answer, \
                on the place the Captain should look at first. To show two files side by side \
                (an interface and its implementation), reveal the second with placement \"beside\".",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace"},
                    "symbol": {"type": "string", "description": "Function, type or variable name to put the cursor on"},
                    "line": {"type": "integer", "minimum": 1, "description": "1-based line; with a symbol, picks the occurrence nearest to it"},
                    "placement": {
                        "type": "string",
                        "enum": ["active", "neighbor", "beside"],
                        "description": "Editor group: \"active\" (default) replaces the current tab's place; \"neighbor\" uses an existing group next to the active one (error if none); \"beside\" uses that neighbor or creates one, keeping the current file visible"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
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
            let params = match parse(&args, cx) {
                Ok(params) => params,
                Err(result) => return Ok(result),
            };
            let Some(editor) = self.feed.as_ref().and_then(ConversationFeed::editor) else {
                return Ok(ToolResult::error(
                    "editor_unavailable",
                    "no editor is connected to this session",
                ));
            };
            let path = args["path"].as_str().unwrap_or_default().to_string();
            let placement = params.placement;
            let symbol = params.symbol.clone();
            let answer = tokio::select! {
                _ = cancel.cancelled() => return Ok(ToolResult::interrupted()),
                answer = tokio::time::timeout(EDITOR_TIMEOUT, editor(params)) => answer,
            };
            let result = match answer {
                Err(_) => {
                    return Ok(ToolResult::error(
                        "editor_timeout",
                        "the editor did not answer",
                    ));
                }
                Ok(Err(e)) => return Ok(ToolResult::error("editor_error", e)),
                Ok(Ok(result)) => result,
            };
            if let Some(error) = result.error {
                return Ok(ToolResult::error("placement_failed", error));
            }
            let mut text = format!("revealed in the editor: {path}");
            if let Some(group) = result.group {
                text.push_str(&format!(" (editor group {group})"));
            }
            if let (Some(symbol), Some(false)) = (&symbol, result.symbol_found) {
                text.push_str(&format!(
                    "; symbol `{symbol}` not found, cursor left at the top or the line"
                ));
            }
            Ok(ToolResult::ok(text).with_metadata(json!({
                "reveal": {"placement": placement, "group": result.group, "symbol_found": result.symbol_found}
            })))
        })
    }
}

/// Validates the arguments: a workspace file, a line >= 1, a known placement.
#[allow(clippy::result_large_err)]
fn parse(args: &Value, cx: &ToolContext) -> Result<RevealParams, ToolResult> {
    let invalid = |m: &str| ToolResult::error("invalid_args", m);
    let path = args["path"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| invalid("path is required"))?;
    let line = match &args["line"] {
        Value::Null => None,
        value => Some(
            value
                .as_u64()
                .filter(|n| *n >= 1)
                .ok_or_else(|| invalid("line must be an integer >= 1"))?,
        ),
    };
    let symbol = args["symbol"]
        .as_str()
        .map(|s| s.trim().trim_end_matches("()").trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let placement = match &args["placement"] {
        Value::Null => RevealPlacement::default(),
        value => serde_json::from_value(value.clone())
            .map_err(|_| invalid("placement must be one of active, neighbor, beside"))?,
    };
    // The editor only opens files of its workspace: the writable roots.
    let resolved = validate_path(&cx.writable_roots, Path::new(path)).map_err(|_| {
        ToolResult::error("outside_workspace", format!("not in the workspace: {path}"))
    })?;
    if !resolved.is_file() {
        return Err(ToolResult::error(
            "not_found",
            format!("no such file: {path}"),
        ));
    }
    Ok(RevealParams {
        path: resolved.to_string_lossy().into_owned(),
        line,
        symbol,
        placement,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::navigator::lsp::protocol::RevealResult;
    use crate::tools::ToolStatus;

    fn cx(root: &Path) -> ToolContext {
        ToolContext {
            secrets: None,
            binding: None,
            workspace: root.to_path_buf(),
            writable_roots: vec![root.to_path_buf()],
            readable_roots: Vec::new(),
        }
    }

    /// A feed whose editor records each request and answers with `answer`.
    fn editor(answer: RevealResult) -> (ConversationFeed, Arc<Mutex<Vec<RevealParams>>>) {
        let feed = ConversationFeed::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        feed.set_editor(Arc::new(move |params| {
            log.lock().unwrap().push(params);
            let answer = answer.clone();
            Box::pin(async move { Ok(answer) })
        }));
        (feed, seen)
    }

    async fn call(tool: &RevealTool, cx: &ToolContext, args: Value) -> ToolResult {
        tool.call(args, CancellationToken::new(), cx).await.unwrap()
    }

    #[tokio::test]
    async fn reveal_sends_the_target_with_the_active_group_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("gstreamer.c"), "int x;\n").unwrap();
        let cx = cx(tmp.path());
        let (feed, seen) = editor(RevealResult {
            group: Some(1),
            symbol_found: Some(true),
            error: None,
        });
        let tool = RevealTool::new(Some(feed));
        let ok = call(
            &tool,
            &cx,
            json!({"path": "gstreamer.c", "symbol": "gstreamer_send_frame()", "line": 3}),
        )
        .await;
        assert_eq!(ok.status, ToolStatus::Ok, "{ok:?}");
        assert!(ok.output.contains("editor group 1"));
        let sent = seen.lock().unwrap().pop().unwrap();
        assert_eq!(sent.symbol.as_deref(), Some("gstreamer_send_frame"));
        assert_eq!(sent.line, Some(3));
        assert_eq!(sent.placement, RevealPlacement::Active);

        let beside = call(
            &tool,
            &cx,
            json!({"path": "gstreamer.c", "placement": "beside"}),
        )
        .await;
        assert_eq!(beside.status, ToolStatus::Ok);
        assert_eq!(
            seen.lock().unwrap().pop().unwrap().placement,
            RevealPlacement::Beside
        );
    }

    #[tokio::test]
    async fn reveal_reports_explicit_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.c"), "").unwrap();
        let cx = cx(tmp.path());
        let code = |r: ToolResult| r.error.unwrap().code;

        let (feed, _) = editor(RevealResult {
            error: Some("no neighbor editor group".into()),
            ..Default::default()
        });
        let tool = RevealTool::new(Some(feed));
        assert_eq!(
            code(call(&tool, &cx, json!({"path": "a.c", "placement": "neighbor"})).await),
            "placement_failed"
        );
        assert_eq!(
            code(call(&tool, &cx, json!({"path": "a.c", "placement": "left"})).await),
            "invalid_args"
        );
        assert_eq!(
            code(call(&tool, &cx, json!({"path": "a.c", "line": 0})).await),
            "invalid_args"
        );
        assert_eq!(
            code(call(&tool, &cx, json!({"path": "nope.c"})).await),
            "not_found"
        );
        assert_eq!(
            code(call(&tool, &cx, json!({"path": "../outside.c"})).await),
            "outside_workspace"
        );

        let unwired = RevealTool::new(Some(ConversationFeed::new()));
        assert_eq!(
            code(call(&unwired, &cx, json!({"path": "a.c"})).await),
            "editor_unavailable"
        );
    }
}
