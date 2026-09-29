//! The only seam to the LLM: a trait producing at most one remark (or
//! silence) for a document state. The real implementation calls the `fast`
//! model alias through the existing provider layer; tests use a scripted mock.

use std::future::Future;
use std::pin::Pin;

use tokio_util::sync::CancellationToken;

use crate::navigator::document::{text_around_changes, unified_diff};
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
        }
    }
}

pub trait Commenter: Send + Sync {
    /// Returns `Ok(Some(remark))`, `Ok(None)` for silence, or an error
    /// (LLM/network failure; surfaced but never fatal).
    fn remark<'a>(
        &'a self,
        ctx: &'a RemarkContext,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Remark>, String>> + Send + 'a>>;
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

/// Error-message prefix marking an answer that violated the prompt
/// contract. The server recognizes it and logs the trigger as `malformed`
/// instead of `provider_error`; the rest of the message is the raw output.
pub const MALFORMED_PREFIX: &str = "malformed answer: ";

/// Commenter calling the fast model alias via the provider layer
/// (non-tool streaming chat call). Holds one HTTP client per provider
/// through a shared [`ClientCache`], so repeated remarks reuse
/// connections instead of rebuilding a client per call.
pub struct LlmCommenter {
    resolved: crate::provider::config::ResolvedModel,
    api_key: String,
    clients: std::sync::Arc<crate::provider::openai::ClientCache>,
}

impl LlmCommenter {
    pub fn new(
        resolved: crate::provider::config::ResolvedModel,
        api_key: String,
        clients: std::sync::Arc<crate::provider::openai::ClientCache>,
    ) -> Self {
        Self {
            resolved,
            api_key,
            clients,
        }
    }

    /// The model identifier sent on the wire (for the call records).
    pub fn model_id(&self) -> &str {
        &self.resolved.model.model
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
}

impl Commenter for LlmCommenter {
    fn remark<'a>(
        &'a self,
        ctx: &'a RemarkContext,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Remark>, String>> + Send + 'a>> {
        Box::pin(async move {
            use crate::provider::openai::{build_request, ChatMessage, ChatRequest};
            use crate::provider::{start_and_consume, StreamDelta};
            let system = ctx
                .prompt
                .as_deref()
                .filter(|prompt| !prompt.trim().is_empty())
                .map(|prompt| format!("{NAVIGATOR_SYSTEM_PROMPT}\n\n{prompt}"))
                .unwrap_or_else(|| NAVIGATOR_SYSTEM_PROMPT.to_string());
            let request = ChatRequest {
                model: self.resolved.model.model.clone(),
                messages: vec![
                    ChatMessage::System { content: system },
                    ChatMessage::User {
                        content: self.build_user_message(ctx),
                    },
                ],
                tools: Vec::new(),
                stream: true,
                stream_options: Some(crate::provider::openai::StreamOptions {
                    include_usage: true,
                }),
                max_tokens: self.resolved.model.max_output_tokens.min(Some(1024)),
                temperature: Some(0.2),
            };
            let client = self
                .clients
                .get(&self.resolved.provider_name, &self.resolved.provider)
                .map_err(|e| e.to_string())?;
            let http = build_request(&client, &self.resolved.provider, &self.api_key, &request);
            let response = http.send().await.map_err(|e| format!("http: {e}"))?;
            let mut noop = |_d: StreamDelta| {};
            let message = start_and_consume(response, cancel, &mut noop)
                .await
                .map_err(|e| e.to_string())?;
            match parse_answer(&message.text) {
                ParsedAnswer::Remark(remark) => Ok(Some(remark)),
                ParsedAnswer::Silent => Ok(None),
                ParsedAnswer::Malformed => {
                    tracing::warn!(raw = %message.text, "navigator: model answer violates the prompt contract");
                    Err(format!("{MALFORMED_PREFIX}{}", message.text))
                }
            }
        })
    }
}

impl LlmCommenter {
    /// The prompted remark: the pipeline branch prompt (rendered from the
    /// templates) frames the metacognition; an empty prompt keeps the
    /// prototype's framing only.
    pub fn remark_with_prompt<'a>(
        &'a self,
        ctx: &'a RemarkContext,
        branch_prompt: &'a str,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Remark>, String>> + Send + 'a>> {
        Box::pin(async move {
            use crate::navigator::commenter::{parse_answer, ParsedAnswer, MALFORMED_PREFIX};
            use crate::provider::openai::{build_request, ChatMessage, ChatRequest};
            use crate::provider::{start_and_consume, StreamDelta};

            let system = if branch_prompt.trim().is_empty() {
                NAVIGATOR_SYSTEM_PROMPT.to_string()
            } else {
                format!("{NAVIGATOR_SYSTEM_PROMPT}\n\n{branch_prompt}")
            };
            let request = ChatRequest {
                model: self.resolved.model.model.clone(),
                messages: vec![
                    ChatMessage::System { content: system },
                    ChatMessage::User {
                        content: self.build_user_message(ctx),
                    },
                ],
                tools: Vec::new(),
                stream: true,
                stream_options: Some(crate::provider::openai::StreamOptions {
                    include_usage: true,
                }),
                max_tokens: self.resolved.model.max_output_tokens.min(Some(1024)),
                temperature: Some(0.2),
            };

            let client = self
                .clients
                .get(&self.resolved.provider_name, &self.resolved.provider)
                .map_err(|e| e.to_string())?;
            let http = build_request(&client, &self.resolved.provider, &self.api_key, &request);
            let response = http.send().await.map_err(|e| format!("http: {e}"))?;

            let mut noop = |_d: StreamDelta| {};
            let message = start_and_consume(response, cancel, &mut noop)
                .await
                .map_err(|e| e.to_string())?;

            match parse_answer(&message.text) {
                ParsedAnswer::Remark(remark) => Ok(Some(remark)),
                ParsedAnswer::Silent => Ok(None),
                ParsedAnswer::Malformed => {
                    tracing::warn!(
                        raw = %message.text,
                        "navigator: model answer violates the prompt contract"
                    );
                    Err(format!("{MALFORMED_PREFIX}{}", message.text))
                }
            }
        })
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
