//! The only seam to the LLM: a trait producing at most one remark (or
//! silence) for a document state. The real implementation calls the `fast`
//! model alias through the existing provider layer; tests use a scripted mock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::navigator::document::{text_around_changes, unified_diff};
use crate::navigator::log::{LogRecord, NavLog};
use crate::navigator::ports::{Metacognition, ModelError};
use crate::navigator::protocol::Remark;

/// Everything the commenter sees about a document.
#[derive(Debug, Clone)]
pub struct RemarkContext {
    pub uri: String,
    pub language_id: String,
    pub diff: String,
    /// Full current text truncated around the changed region.
    pub text_excerpt: String,
    pub diagnostics: Vec<Diag>,
    /// The user's current free-text goal, if any.
    pub intent: Option<String>,
    /// Pipeline-selected metacognition prompt, already template-rendered.
    pub prompt: Option<String>,
    /// The workspace root; unlocks the read/grep tools for the metacognition.
    pub workspace: Option<PathBuf>,
    /// Open editor buffers keyed by absolute path: `read` answers from the
    /// buffer (unsaved edits), not from disk.
    pub open_buffers: BTreeMap<PathBuf, String>,
}

/// A compiler/editor diagnostic forwarded by the extension.
#[derive(Debug, Clone)]
pub struct Diag {
    pub line: u32,
    pub severity: String,
    pub message: String,
}

impl RemarkContext {
    pub fn build(
        uri: &str,
        language_id: &str,
        baseline: &str,
        current: &str,
        diagnostics: Vec<Diag>,
        intent: Option<String>,
    ) -> Self {
        Self {
            uri: uri.to_string(),
            language_id: language_id.to_string(),
            diff: unified_diff(baseline, current),
            text_excerpt: text_around_changes(baseline, current, 20),
            diagnostics,
            intent,
            prompt: None,
            workspace: None,
            open_buffers: BTreeMap::new(),
        }
    }

    /// Builder seam: stamps workspace and open buffers on a context built
    /// by [`RemarkContext::build`] (the server fills them at its two
    /// `RemarkContext` sites).
    pub fn into_context(
        mut self,
        workspace: PathBuf,
        open_buffers: BTreeMap<PathBuf, String>,
    ) -> Self {
        self.workspace = Some(workspace);
        self.open_buffers = open_buffers;
        self
    }
}

/// What the metacognition's answer parsed to: a remark, explicit silence,
/// or output that violates the prompt contract (non-empty but unparseable).
/// Malformed output is not silence: the log records it as `malformed` so
/// prompt regressions are visible instead of silently eaten.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedAnswer {
    Remark(Remark),
    Silent,
    Malformed,
}

/// Parses the model's answer per the prompt contract: JSON only, either
/// `{"silent": true}` or `{"line": <1-based>, "text": "..."}`. Tolerates code
/// fences and surrounding prose; empty output counts as silence.
pub fn parse_answer(raw: &str) -> ParsedAnswer {
    if raw.trim().is_empty() {
        return ParsedAnswer::Silent;
    }
    let Some(json_str) = extract_json(raw) else {
        return ParsedAnswer::Malformed;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str.trim()) else {
        return ParsedAnswer::Malformed;
    };
    if v.get("silent").and_then(|s| s.as_bool()).unwrap_or(false) {
        return ParsedAnswer::Silent;
    }
    let Some(line) = v.get("line").and_then(|l| l.as_u64()) else {
        return ParsedAnswer::Malformed;
    };
    if line == 0 {
        return ParsedAnswer::Malformed;
    }
    let Some(text) = v.get("text").and_then(|t| t.as_str()) else {
        return ParsedAnswer::Malformed;
    };
    if text.trim().is_empty() {
        return ParsedAnswer::Malformed;
    }
    match u32::try_from(line - 1) {
        Ok(line0) => ParsedAnswer::Remark(Remark {
            // model speaks 1-based; wire speaks 0-based
            line: line0,
            text: text.trim().to_string(),
        }),
        Err(_) => ParsedAnswer::Malformed,
    }
}

/// Extracts the innermost balanced JSON object from prose / code fences.
fn extract_json(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    // Direct hit first.
    if trimmed.starts_with('{') && serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed);
    }
    // Otherwise find the first balanced { ... } and try it.
    let bytes = trimmed.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&trimmed[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The system prompt framing the navigator. Public for tests.
pub const NAVIGATOR_SYSTEM_PROMPT: &str = "\
You are a senior pair-programming navigator watching over the driver's shoulder.

Your job is NOT to write code. You never rewrite, never produce code blocks, \
never refactor. You observe the current diff and, only when something is \
genuinely worth interrupting for, you say one or two short sentences anchored \
to a specific line.

Silence is the expected default. Most pauses are fine: the code is reasonable, \
or the driver is mid-thought. Speak up only for a real risk: a bug being \
introduced, a missing error path, an edge case the diff ignores, a mismatch \
with the driver's stated intent, or a much simpler alternative to what is \
being written.

Answer with JSON only, no prose around it, either:
{\"silent\": true}
or:
{\"line\": <1-based line number in the current file>, \"text\": \"<one or two short sentences>\"}";

/// Commenter calling the fast model alias via the provider layer
/// (streaming chat call with a small read/grep tool loop). Holds one HTTP
/// client per provider through a shared [`ClientCache`], so repeated
/// remarks reuse connections instead of rebuilding a client per call.
pub struct LlmCommenter {
    resolved: crate::provider::config::ResolvedModel,
    api_key: String,
    clients: Arc<crate::provider::openai::ClientCache>,
    log: Option<NavLog>,
}

/// The whole remark (tool rounds + final answer) is capped at this budget;
/// past it the metacognition stays silent.
const REMARK_BUDGET: Duration = Duration::from_secs(8);
/// At most this many rounds where the model may call tools; the call after
/// the last one carries no tools, forcing the JSON answer.
const MAX_TOOL_ROUNDS: usize = 2;
/// Cap on the tokens of each metacognition round: a remark is a sentence or
/// two, so a runaway generation is cost with no value.
const REMARK_MAX_TOKENS: u64 = 1024;
/// Cap on each tool output sent back to the model.
const TOOL_OUTPUT_MAX_CHARS: usize = 4000;

impl LlmCommenter {
    pub fn new(
        resolved: crate::provider::config::ResolvedModel,
        api_key: String,
        clients: Arc<crate::provider::openai::ClientCache>,
    ) -> Self {
        Self {
            resolved,
            api_key,
            clients,
            log: None,
        }
    }

    /// Attaches the navigator log so each round and tool execution is
    /// recorded as a `Call`.
    pub fn with_log(mut self, log: NavLog) -> Self {
        self.log = Some(log);
        self
    }

    /// The model identifier sent on the wire (for the call records).
    pub fn model_id(&self) -> &str {
        &self.resolved.model.model
    }

    fn log_call(&self, layer: &str, model: &str, started: Instant, cancelled: bool) {
        if let Some(log) = &self.log {
            log.append(&LogRecord::Call {
                layer,
                model,
                latency_ms: started.elapsed().as_millis(),
                input_tokens: None,
                output_tokens: None,
                cancelled,
                purpose: None,
            });
        }
    }

    fn build_user_message(&self, ctx: &RemarkContext) -> String {
        let mut m = String::new();
        m.push_str(&format!(
            "File: {}\nLanguage: {}\n",
            ctx.uri, ctx.language_id
        ));
        if let Some(intent) = &ctx.intent {
            m.push_str(&format!("Driver's stated intent: {intent}\n"));
        }
        if !ctx.diagnostics.is_empty() {
            m.push_str("Current diagnostics:\n");
            for d in &ctx.diagnostics {
                m.push_str(&format!(
                    "  line {}: [{}] {}\n",
                    d.line + 1,
                    d.severity,
                    d.message
                ));
            }
        }
        m.push_str("\nUnified diff since the last remark:\n");
        m.push_str(&ctx.diff);
        m.push_str("\n\nCurrent text around the changed region:\n");
        m.push_str(&ctx.text_excerpt);
        m.push_str("\n\nDecide: stay silent, or give one short remark. JSON only.");
        m
    }

    /// The shared remark loop: up to [`MAX_TOOL_ROUNDS`] rounds offering
    /// `read`/`grep`, then one final call without tools to force the JSON
    /// answer. Everything runs under one [`REMARK_BUDGET`] timeout; past it
    /// the metacognition stays silent.
    async fn remark_with_tools(
        &self,
        ctx: &RemarkContext,
        cancel: &CancellationToken,
    ) -> Result<Option<Remark>, ModelError> {
        use crate::provider::openai::{build_request, ChatMessage, ChatRequest};
        use crate::provider::{start_and_consume, StreamDelta};

        let body = match tokio::time::timeout(REMARK_BUDGET, async {
            // The pipeline renders the branch prompt (conversation, live
            // state) into the context; without it the metacognition would
            // judge the diff alone.
            let branch_prompt = ctx.prompt.as_deref().unwrap_or("");
            let system = if branch_prompt.trim().is_empty() {
                NAVIGATOR_SYSTEM_PROMPT.to_string()
            } else {
                format!("{NAVIGATOR_SYSTEM_PROMPT}\n\n{branch_prompt}")
            };
            let tools = ctx
                .workspace
                .as_ref()
                .map(|ws| vec![tool_def_read(ws), tool_def_grep(ws)]);
            let mut messages = vec![
                ChatMessage::System {
                    content: system.clone(),
                },
                ChatMessage::User {
                    content: self.build_user_message(ctx),
                },
            ];

            let client = self
                .clients
                .get(&self.resolved.provider_name, &self.resolved.provider)?;

            let mut message = None;
            for round in 0..=MAX_TOOL_ROUNDS {
                // The final round carries no tools, forcing the JSON answer.
                let round_tools = if round < MAX_TOOL_ROUNDS {
                    tools.clone().unwrap_or_default()
                } else {
                    Vec::new()
                };
                let request = ChatRequest {
                    model: self.resolved.model.model.clone(),
                    messages: messages.clone(),
                    tools: round_tools,
                    stream: true,
                    stream_options: Some(crate::provider::openai::StreamOptions {
                        include_usage: true,
                    }),
                    // `None.min(Some(n))` is `None`: a model without a stated
                    // limit would run uncapped, so the cap is applied explicitly.
                    max_tokens: Some(
                        self.resolved
                            .model
                            .max_output_tokens
                            .map_or(REMARK_MAX_TOKENS, |m| m.min(REMARK_MAX_TOKENS)),
                    ),
                    temperature: Some(0.2),
                };
                let http = build_request(&client, &self.resolved.provider, &self.api_key, &request);
                let started = Instant::now();
                let response = match http.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        self.log_call("metacognition_round", self.model_id(), started, false);
                        return Err(crate::error::ProviderError::Http(e).into());
                    }
                };
                let mut noop = |_d: StreamDelta| {};
                let msg = match start_and_consume(response, cancel, &mut noop).await {
                    Ok(m) => m,
                    Err(e) => {
                        let cancelled = matches!(e, crate::error::ProviderError::Canceled { .. });
                        self.log_call("metacognition_round", self.model_id(), started, cancelled);
                        return Err(e.into());
                    }
                };
                self.log_call("metacognition_round", self.model_id(), started, false);

                if round == MAX_TOOL_ROUNDS || msg.tool_calls.is_empty() {
                    message = Some(msg);
                    break;
                }
                // Feed the assistant turn (tool calls) back, execute each
                // tool, append its result.
                messages.push(ChatMessage::Assistant {
                    content: if msg.text.is_empty() {
                        None
                    } else {
                        Some(msg.text.clone())
                    },
                    reasoning_content: None,
                    tool_calls: msg
                        .tool_calls
                        .iter()
                        .map(|tc| crate::provider::openai::RequestToolCall {
                            id: tc.id.clone(),
                            kind: "function".to_string(),
                            function: crate::provider::openai::RequestFunction {
                                name: tc.name.clone(),
                                arguments: tc.args.to_string(),
                            },
                        })
                        .collect(),
                });
                for tc in &msg.tool_calls {
                    let output = self.execute_tool(ctx, tc).await;
                    messages.push(ChatMessage::Tool {
                        tool_call_id: tc.id.clone(),
                        content: output,
                    });
                }
            }
            let message = message.expect("the loop always sets the final message");
            Ok::<_, ModelError>(message)
        })
        .await
        {
            Ok(body) => body?,
            Err(_) => {
                tracing::warn!(
                    budget_ms = REMARK_BUDGET.as_millis() as u64,
                    "navigator: metacognition remark over budget, staying silent"
                );
                if let Some(log) = &self.log {
                    log.append(&LogRecord::Call {
                        layer: "metacognition_round",
                        model: self.model_id(),
                        latency_ms: REMARK_BUDGET.as_millis(),
                        input_tokens: None,
                        output_tokens: None,
                        cancelled: true,
                        purpose: None,
                    });
                }
                return Ok(None);
            }
        };

        match parse_answer(&body.text) {
            ParsedAnswer::Remark(remark) => Ok(Some(remark)),
            ParsedAnswer::Silent => Ok(None),
            ParsedAnswer::Malformed => {
                tracing::warn!(raw = %body.text, "navigator: model answer violates the prompt contract");
                Err(ModelError::Malformed(body.text))
            }
        }
    }

    /// Executes one tool call from the model: `read` (with the open-buffer
    /// override) or `grep`, through the shared `Tool` implementations.
    /// Relative paths anchor to the workspace first, so the model may pass
    /// either form.
    async fn execute_tool(&self, ctx: &RemarkContext, tc: &crate::provider::ToolCall) -> String {
        use crate::tools::native::{GrepTool, ReadTool};
        use crate::tools::{Tool, ToolContext};

        let started = Instant::now();
        let Some(workspace) = ctx.workspace.clone() else {
            return "error: no workspace; tools are unavailable".to_string();
        };
        let mut args = tc.args.clone();
        if let Some(path) = args.get("path").and_then(|p| p.as_str()) {
            if !path.starts_with('/') {
                args["path"] =
                    serde_json::Value::String(workspace.join(path).to_string_lossy().into_owned());
            }
        }
        // Reading requires the root in `validate_path`'s roots; the
        // metacognition never writes, so the workspace root is the only
        // root it gets.
        let cx = ToolContext {
            workspace: workspace.clone(),
            writable_roots: vec![workspace.clone()],
        };
        let output = match tc.name.as_str() {
            "read" => {
                // An open buffer wins over disk: unsaved edits are what
                // the driver actually sees.
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
        self.log_call("metacognition_tool", &tc.name, started, false);
        crate::navigator::log::excerpt(&text, TOOL_OUTPUT_MAX_CHARS)
    }
}

/// The system prompt for the reflex-context refresh.
const REFLEX_CONTEXT_PROMPT: &str = include_str!("reflex_context_prompt.md");
/// The refresh is capped well under the remark budget's tool loop: one
/// short plain call.
const REFLEX_CONTEXT_BUDGET: Duration = Duration::from_secs(15);
const REFLEX_CONTEXT_MAX_TOKENS: u64 = 300;

impl crate::navigator::ports::ReflexContextModel for LlmCommenter {
    fn refresh<'a>(
        &'a self,
        request: &'a crate::navigator::ports::ReflexContextRequest,
    ) -> BoxFuture<'a, Result<String, ModelError>> {
        use crate::provider::openai::{build_request, ChatMessage, ChatRequest, StreamOptions};
        use crate::provider::{start_and_consume, StreamDelta};

        Box::pin(async move {
            let user = format!(
                "File: {}\nLanguage: {}\n\nIntent on record:\n{}\n\nRecent conversation:\n{}\n\n\
                 Excerpt around the cursor:\n{}\n",
                request.file,
                request.language_id,
                or_none(&request.intent),
                or_none(&request.conversation),
                request.excerpt,
            );
            let chat = ChatRequest {
                model: self.resolved.model.model.clone(),
                messages: vec![
                    ChatMessage::System {
                        content: REFLEX_CONTEXT_PROMPT.to_string(),
                    },
                    ChatMessage::User { content: user },
                ],
                tools: Vec::new(),
                stream: true,
                stream_options: Some(StreamOptions {
                    include_usage: true,
                }),
                max_tokens: self
                    .resolved
                    .model
                    .max_output_tokens
                    .min(Some(REFLEX_CONTEXT_MAX_TOKENS))
                    .or(Some(REFLEX_CONTEXT_MAX_TOKENS)),
                temperature: Some(0.2),
            };
            let client = self
                .clients
                .get(&self.resolved.provider_name, &self.resolved.provider)?;
            let cancel = CancellationToken::new();
            let call = async {
                let http = build_request(&client, &self.resolved.provider, &self.api_key, &chat);
                let response = http
                    .send()
                    .await
                    .map_err(crate::error::ProviderError::Http)?;
                let mut noop = |_d: StreamDelta| {};
                start_and_consume(response, &cancel, &mut noop)
                    .await
                    .map_err(ModelError::Provider)
            };
            match tokio::time::timeout(REFLEX_CONTEXT_BUDGET, call).await {
                Ok(msg) => Ok(msg?.text.trim().to_string()),
                Err(_) => {
                    cancel.cancel();
                    Err(ModelError::Provider(
                        crate::error::ProviderError::Canceled { partial: None },
                    ))
                }
            }
        })
    }
}

fn or_none(text: &str) -> &str {
    if text.trim().is_empty() {
        "(none)"
    } else {
        text
    }
}

/// `read` tool definition with workspace-anchored path hint.
fn tool_def_read(ws: &Path) -> crate::provider::openai::ToolDef {
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
fn tool_def_grep(ws: &Path) -> crate::provider::openai::ToolDef {
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

impl Metacognition for LlmCommenter {
    fn remark<'a>(
        &'a self,
        ctx: &'a RemarkContext,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<Remark>, ModelError>> {
        Box::pin(self.remark_with_tools(ctx, cancel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_remark() {
        assert_eq!(
            parse_answer(r#"{"line": 12, "text": "careful, missing error path"}"#),
            ParsedAnswer::Remark(Remark {
                line: 11,
                text: "careful, missing error path".to_string()
            })
        );
    }

    #[test]
    fn parses_silent() {
        assert_eq!(parse_answer(r#"{"silent": true}"#), ParsedAnswer::Silent);
    }

    #[test]
    fn garbage_is_malformed_not_silence() {
        assert_eq!(
            parse_answer("I think you should refactor this"),
            ParsedAnswer::Malformed
        );
        assert_eq!(parse_answer(""), ParsedAnswer::Silent);
    }

    #[test]
    fn json_without_silent_or_line_is_malformed() {
        assert_eq!(
            parse_answer(r#"{"silent": false}"#),
            ParsedAnswer::Malformed
        );
        assert_eq!(parse_answer(r#"{"other": 1}"#), ParsedAnswer::Malformed);
    }

    #[test]
    fn parse_model_answer_folds_malformed_to_silence() {
        assert_eq!(parse_answer("nope"), ParsedAnswer::Malformed);
        assert_eq!(
            parse_answer(r#"{"line": 0, "text": "x"}"#),
            ParsedAnswer::Malformed
        );
        assert_eq!(
            parse_answer(r#"{"line": 1, "text": "  "}"#),
            ParsedAnswer::Malformed
        );
    }

    #[test]
    fn json_in_code_fences_is_parsed() {
        let raw = "```json\n{\"line\": 3, \"text\": \"off by one\"}\n```";
        assert_eq!(
            parse_answer(raw),
            ParsedAnswer::Remark(Remark {
                line: 2,
                text: "off by one".to_string()
            })
        );
    }

    #[test]
    fn json_wrapped_in_prose_is_parsed() {
        let raw = "Here is my answer:\n{\"line\": 7, \"text\": \"edge case\"}\nHope it helps";
        assert_eq!(
            parse_answer(raw),
            ParsedAnswer::Remark(Remark {
                line: 6,
                text: "edge case".to_string()
            })
        );
    }

    #[test]
    fn system_prompt_forbids_code_and_encourages_silence() {
        assert!(NAVIGATOR_SYSTEM_PROMPT.contains("never rewrite"));
        assert!(NAVIGATOR_SYSTEM_PROMPT.contains("Silence is the expected default"));
        assert!(NAVIGATOR_SYSTEM_PROMPT.contains("JSON only"));
    }

    #[test]
    fn context_builds_diff_and_excerpt() {
        let ctx = RemarkContext::build(
            "file:///a.rs",
            "rust",
            "a\nb\nc\n",
            "a\nB\nc\nd\n",
            vec![Diag {
                line: 1,
                severity: "error".into(),
                message: "boom".into(),
            }],
            Some("fix the parser".into()),
        );
        assert!(ctx.diff.contains("B"));
        assert!(ctx.text_excerpt.contains("a"));
        assert_eq!(ctx.diagnostics.len(), 1);
        assert_eq!(ctx.intent.as_deref(), Some("fix the parser"));
    }
}
