//! The metacognition layer: the single [`Metacognition`] implementation
//! ([`LlmCommenter`], a fast chat model with read-only tools) and the
//! producer of the reflex context. Tests script the seam with a mock.
//!
//! This file owns the commenter and its remark loop; `answer` parses the
//! model's reply, `tools` runs the read/grep tools, `reflex_context` holds
//! the reflex-context producer.

mod answer;
mod permission;
mod reflex_context;
mod tools;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::navigator::document::{text_around_changes, unified_diff};
use crate::navigator::log::{LogRecord, NavLog};
use crate::navigator::lsp::protocol::Remark;
use crate::navigator::ports::{Metacognition, ModelError};

use answer::tool_attempt_instead_of_answer;
pub use answer::{parse_answer, ParsedAnswer};
pub use tools::slice_buffer;
use tools::{tool_def_grep, tool_def_read};

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
    /// The other folders of a multi-root workspace: readable like the
    /// workspace itself, relative paths still anchor on the workspace root.
    pub extra_roots: Vec<PathBuf>,
    /// Open editor buffers keyed by absolute path: `read` answers from the
    /// buffer (unsaved edits), not from disk.
    pub open_buffers: BTreeMap<PathBuf, String>,
    /// Rounds where the model may call tools before it must answer.
    pub tool_rounds: usize,
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
            extra_roots: Vec::new(),
            open_buffers: BTreeMap::new(),
            tool_rounds: REMARK_TOOL_ROUNDS,
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

/// The answer format, shared by the system prompt and the reminders sent
/// when the model has to answer without tools.
macro_rules! answer_contract {
    () => {
        "\
Answer with JSON only, no prose around it, either:
{\"silent\": true}
or:
{\"line\": <1-based line number in the current file>, \"text\": \"<one or two short sentences>\"}"
    };
}

/// The system prompt framing the navigator. Public for tests.
pub const NAVIGATOR_SYSTEM_PROMPT: &str = concat!(
    "\
You are a senior pair-programming navigator watching over the Captain's shoulder.

Your job is NOT to write code. You never rewrite, never produce code blocks, \
never refactor. You observe the current diff and, only when something is \
genuinely worth interrupting for, you say one or two short sentences anchored \
to a specific line.

Silence is the expected default. Most pauses are fine: the code is reasonable, \
or the Captain is mid-thought. Speak up only for a real risk: a bug being \
introduced, a missing error path, an edge case the diff ignores, a mismatch \
with the Captain's stated intent, or a much simpler alternative to what is \
being written.

",
    answer_contract!()
);

/// The closing instruction of the user turn.
const DECIDE_INSTRUCTION: &str = "Decide: stay silent, or give one short remark. JSON only.";

/// Appended when the model has no tools left (the last round, or the retry
/// after it tried a tool anyway): it must answer with what it already read.
const NO_MORE_TOOLS_NOTICE: &str = concat!(
    "No more tools are available: do not call any tool and do not write tool-call markup. \
Answer now, using only what you have already read.

",
    answer_contract!()
);

/// Commenter calling the fast model alias via the provider layer
/// (streaming chat call with a small read/grep tool loop). Holds one HTTP
/// client per provider through a shared [`ClientCache`], so repeated
/// remarks reuse connections instead of rebuilding a client per call.
pub struct LlmCommenter {
    resolved: crate::provider::config::ResolvedModel,
    api_key: String,
    clients: Arc<crate::provider::openai::ClientCache>,
    /// The alias's default variant, merged into every request.
    variant: Option<crate::provider::config::Variant>,
    log: Option<NavLog>,
}

/// The whole remark (tool rounds + final answer) is capped at this budget;
/// past it the metacognition stays silent.
const REMARK_BUDGET: Duration = Duration::from_secs(20);
/// Tool rounds of an ordinary remark: one optional round of reading, then the
/// answer. Each round is a model round trip the Captain waits for, and an
/// ordinary remark rests on the diff it was given. The call after the last
/// round carries no tools, forcing the JSON answer.
pub const REMARK_TOOL_ROUNDS: usize = 1;
/// Tool rounds of a remark answering a call for help: the Captain asked, so
/// the model may read more before it speaks.
pub const HELP_TOOL_ROUNDS: usize = 3;
/// Cap on the tokens of each metacognition round: a remark is a sentence or
/// two, so a runaway generation is cost with no value.
const REMARK_MAX_TOKENS: u64 = 1024;

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
            variant: None,
            log: None,
        }
    }

    /// Applies `variant` (the alias's default, if it has one) to every
    /// request this commenter sends.
    pub fn with_variant(mut self, variant: Option<crate::provider::config::Variant>) -> Self {
        self.variant = variant;
        self
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

    fn log_call(
        &self,
        layer: &str,
        model: &str,
        started: Instant,
        cancelled: bool,
        usage: Option<crate::provider::Usage>,
    ) {
        if let Some(log) = &self.log {
            log.append(&LogRecord::Call {
                layer,
                model,
                latency_ms: started.elapsed().as_millis(),
                input_tokens: usage.map(|u| u.prompt_tokens),
                output_tokens: usage.map(|u| u.completion_tokens),
                cancelled,
                purpose: None,
            });
        }
    }

    /// The user turn. When the pipeline rendered a branch prompt (it already
    /// carries the file, intent, diff, context and diagnostics, and goes into
    /// the system message) only the instruction to decide is left; repeating
    /// the context would double the input of every round.
    fn build_user_message(&self, ctx: &RemarkContext) -> String {
        if ctx.prompt.as_deref().is_some_and(|p| !p.trim().is_empty()) {
            return DECIDE_INSTRUCTION.to_string();
        }
        let mut m = String::new();
        m.push_str(&format!(
            "File: {}\nLanguage: {}\n",
            ctx.uri, ctx.language_id
        ));
        if let Some(intent) = &ctx.intent {
            m.push_str(&format!("Captain's stated intent: {intent}\n"));
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
        m.push_str("\n\n");
        m.push_str(DECIDE_INSTRUCTION);
        m
    }

    /// One chat round: sends `messages` with `tools`, consumes the stream and
    /// logs the round.
    async fn chat_round(
        &self,
        messages: &[crate::provider::openai::ChatMessage],
        tools: Vec<crate::provider::openai::ToolDef>,
        cancel: &CancellationToken,
    ) -> Result<crate::provider::AssistantMessage, ModelError> {
        self.chat_round_capped(messages, tools, cancel, REMARK_MAX_TOKENS)
            .await
    }

    async fn chat_round_capped(
        &self,
        messages: &[crate::provider::openai::ChatMessage],
        tools: Vec<crate::provider::openai::ToolDef>,
        cancel: &CancellationToken,
        output_cap: u64,
    ) -> Result<crate::provider::AssistantMessage, ModelError> {
        use crate::provider::client::ModelClient;
        use crate::provider::openai::ChatRequest;
        use crate::provider::{start_and_consume, StreamDelta};

        let request = ChatRequest {
            model: self.resolved.model.model.clone(),
            messages: messages.to_vec(),
            tools,
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
                    .map_or(output_cap, |m| m.min(output_cap)),
            ),
            temperature: Some(0.2),
            extra: self.variant.clone().unwrap_or_default(),
        };
        let client = ModelClient::new(
            &self.resolved.provider_name,
            &self.resolved.provider,
            self.api_key.clone(),
            &self.clients,
        )?;
        // Keep the navigator's verbatim variant and JSON-before-custom-headers
        // order, rather than applying chat's variant and send cancellation.
        let http = client.prepare_request(client.post("chat/completions").json(&request));
        let started = Instant::now();
        let response = match http.send().await {
            Ok(r) => r,
            Err(e) => {
                self.log_call("metacognition_round", self.model_id(), started, false, None);
                return Err(crate::error::ProviderError::Http(e).into());
            }
        };
        let mut noop = |_d: StreamDelta| {};
        match start_and_consume(response, cancel, &mut noop).await {
            Ok(msg) => {
                self.log_call(
                    "metacognition_round",
                    self.model_id(),
                    started,
                    false,
                    msg.usage,
                );
                Ok(msg)
            }
            Err(e) => {
                let cancelled = matches!(e, crate::error::ProviderError::Canceled { .. });
                self.log_call(
                    "metacognition_round",
                    self.model_id(),
                    started,
                    cancelled,
                    None,
                );
                Err(e.into())
            }
        }
    }

    /// The shared remark loop: up to `ctx.tool_rounds` rounds offering
    /// `read`/`grep`, then one final call without tools to force the JSON
    /// answer. A final answer that is unparseable because the model tried a
    /// tool anyway is asked for once more. Everything runs under one
    /// [`REMARK_BUDGET`] timeout; past it the metacognition stays silent.
    async fn remark_with_tools(
        &self,
        ctx: &RemarkContext,
        cancel: &CancellationToken,
    ) -> Result<Option<Remark>, ModelError> {
        use crate::provider::openai::ChatMessage;

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
                ChatMessage::System { content: system },
                ChatMessage::User { provenance: Default::default(),
                    content: self.build_user_message(ctx),
                },
            ];

            let mut message = None;
            for round in 0..=ctx.tool_rounds {
                let last = round == ctx.tool_rounds;
                // The final round carries no tools, forcing the JSON answer,
                // and says so.
                let round_tools = if last {
                    messages.push(ChatMessage::User { provenance: Default::default(),
                        content: NO_MORE_TOOLS_NOTICE.to_string(),
                    });
                    Vec::new()
                } else {
                    tools.clone().unwrap_or_default()
                };
                let msg = self
                    .chat_round(&messages, round_tools, cancel)
                    .await?;

                if last || msg.tool_calls.is_empty() {
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
answered_by: None, });
                }
            }
            let mut message = message.expect("the loop always sets the final message");
            // The model wrote a tool call where the answer belongs: the
            // leaked call is never run; the model is asked once more.
            if tool_attempt_instead_of_answer(&message) {
                tracing::warn!(raw = %message.text, "navigator: final answer tried a tool, asking again");
                messages.push(ChatMessage::User { provenance: Default::default(),
                    content: format!(
                        "Your last reply tried to call a tool. {NO_MORE_TOOLS_NOTICE}"
                    ),
                });
                message = self.chat_round(&messages, Vec::new(), cancel).await?;
            }
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
