//! The reflex-facing calls of the runtime: the inline-completion request, the
//! reflex context served in front of the code, its background refresh and
//! the reflex call record.

use std::sync::Arc;

use tokio::time::Instant;

use crate::navigator::log::LogRecord;
use crate::navigator::lsp::protocol::{InlineCompletionParams, InlineCompletionResult, Position};
use crate::navigator::reflex::to_result;

use super::Runtime;

impl Runtime {
    /// `navigator/inlineCompletion`: ghost-text request from the extension's
    /// inline-completion provider. The reflex layer answers or returns an
    /// empty item list.
    pub(crate) async fn inline_completion(
        self: &Arc<Self>,
        params: InlineCompletionParams,
    ) -> InlineCompletionResult {
        let Some(reflex) = &self.reflex else {
            return InlineCompletionResult::default();
        };
        let document = self.documents.lock().await.text_and_language(&params.uri);
        let Some((text, language_id)) = document else {
            return InlineCompletionResult::default();
        };
        let pos = Position {
            line: params.line,
            character: params.character,
        };
        // The reflex context: the metacognition's refreshed text, else the
        // intent summary. The refresh runs in the background; the service
        // caps the text.
        let context = self.reflex_context_text().await;
        self.refresh_reflex_context(&params.uri, Some(params.line))
            .await;
        let started = Instant::now();
        match reflex
            .suggest(
                &params.uri,
                &text,
                params.version,
                pos,
                &context,
                &language_id,
            )
            .await
        {
            Ok(suggestion) => {
                self.log_reflex_call(started.elapsed().as_millis());
                to_result(&suggestion)
            }
            Err(outcome) => {
                // A debounced or out-of-range request never reached the
                // model: logging it would bill a call that did not happen.
                if outcome.called_model() {
                    self.log_reflex_call(started.elapsed().as_millis());
                }
                InlineCompletionResult::default()
            }
        }
    }

    /// One reflex layer model call, for the cost log. Usage tokens are not
    /// surfaced by the service seam yet; latency is.
    fn log_reflex_call(&self, latency_ms: u128) {
        if let Some(model) = self.reflex_model.as_deref() {
            self.log.append(&LogRecord::Call {
                layer: "reflex",
                model,
                latency_ms,
                input_tokens: None,
                output_tokens: None,
                cancelled: false,
                purpose: None,
            });
        }
    }

    /// The text served as the reflex context: the metacognition's refreshed
    /// text once one exists, else the intent summary.
    async fn reflex_context_text(&self) -> String {
        if let Some(text) = self.reflex_context.text().await {
            return text;
        }
        self.current_intent().await.0
    }

    /// Starts a background refresh of the reflex context when it is due.
    pub(super) async fn refresh_reflex_context(
        self: &Arc<Self>,
        uri: &str,
        cursor_line: Option<u32>,
    ) {
        if !self.reflex_context.is_enabled() {
            return;
        }
        let (intent, _) = self.current_intent().await;
        let conversation = self
            .conversation
            .as_ref()
            .map(|feed| feed.digest())
            .unwrap_or_default();
        let Some(claim) = self
            .reflex_context
            .claim(uri, cursor_line, &intent, &conversation)
            .await
        else {
            return;
        };
        let document = self.documents.lock().await.text_and_language(uri);
        let Some((text, language_id)) = document else {
            self.reflex_context.release().await;
            return;
        };
        self.reflex_context
            .spawn_refresh(claim, uri, intent, conversation, language_id, &text);
    }
}
