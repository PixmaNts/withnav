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

/// Parses the model's answer per the prompt contract: JSON only, either
/// `{"silent": true}` or `{"line": <1-based>, "text": "..."}`. Tolerates code
/// fences and surrounding prose; anything unparseable is None (silence).
pub fn parse_model_answer(raw: &str) -> Option<Remark> {
    let json_str = extract_json(raw)?;
    let v: serde_json::Value = serde_json::from_str(json_str.trim()).ok()?;
    if v.get("silent").and_then(|s| s.as_bool()).unwrap_or(false) {
        return None;
    }
    let line = v.get("line").and_then(|l| l.as_u64())?;
    if line == 0 {
        return None;
    }
    let text = v.get("text").and_then(|t| t.as_str())?;
    if text.trim().is_empty() {
        return None;
    }
    Some(Remark {
        // model speaks 1-based; wire speaks 0-based
        line: u32::try_from(line - 1).ok()?,
        text: text.trim().to_string(),
    })
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
/// (non-tool streaming chat call).
pub struct LlmCommenter {
    resolved: crate::provider::config::ResolvedModel,
    api_key: String,
}

impl LlmCommenter {
    pub fn new(resolved: crate::provider::config::ResolvedModel, api_key: String) -> Self {
        Self { resolved, api_key }
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
            use crate::provider::openai::{build_client, build_request, ChatMessage, ChatRequest};
            use crate::provider::{start_and_consume, StreamDelta};

            let request = ChatRequest {
                model: self.resolved.model.model.clone(),
                messages: vec![
                    ChatMessage::System {
                        content: NAVIGATOR_SYSTEM_PROMPT.to_string(),
                    },
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

            let client = build_client(&self.resolved.provider).map_err(|e| e.to_string())?;
            let http = build_request(&client, &self.resolved.provider, &self.api_key, &request);
            let response = http.send().await.map_err(|e| format!("http: {e}"))?;

            let mut noop = |_d: StreamDelta| {};
            let message = start_and_consume(response, cancel, &mut noop)
                .await
                .map_err(|e| e.to_string())?;

            let remark = parse_model_answer(&message.text);
            if remark.is_none() && !message.text.trim().is_empty() {
                tracing::warn!(
                    raw = %message.text,
                    "navigator: unparseable model answer treated as silence"
                );
            }
            Ok(remark)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remark(line0: u32, text: &str) -> Option<Remark> {
        Some(Remark {
            line: line0,
            text: text.to_string(),
        })
    }

    #[test]
    fn parses_valid_remark() {
        assert_eq!(
            parse_model_answer(r#"{"line": 12, "text": "careful, missing error path"}"#),
            remark(11, "careful, missing error path")
        );
    }

    #[test]
    fn parses_silent() {
        assert_eq!(parse_model_answer(r#"{"silent": true}"#), None);
    }

    #[test]
    fn garbage_is_silence() {
        assert_eq!(parse_model_answer("I think you should refactor this"), None);
        assert_eq!(parse_model_answer(""), None);
    }

    #[test]
    fn json_in_code_fences_is_parsed() {
        let raw = "```json\n{\"line\": 3, \"text\": \"off by one\"}\n```";
        assert_eq!(parse_model_answer(raw), remark(2, "off by one"));
    }

    #[test]
    fn json_wrapped_in_prose_is_parsed() {
        let raw = "Here is my answer:\n{\"line\": 7, \"text\": \"edge case\"}\nHope it helps";
        assert_eq!(parse_model_answer(raw), remark(6, "edge case"));
    }

    #[test]
    fn zero_line_or_empty_text_rejected() {
        assert_eq!(parse_model_answer(r#"{"line": 0, "text": "x"}"#), None);
        assert_eq!(parse_model_answer(r#"{"line": 1, "text": "  "}"#), None);
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
