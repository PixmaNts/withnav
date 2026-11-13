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
    /// `withnav/inlineCompletion`: ghost-text request from the extension's
    /// inline-completion provider. The reflex layer answers or returns an
    /// empty item list.
    pub(crate) async fn inline_completion(
        self: &Arc<Self>,
        params: InlineCompletionParams,
    ) -> InlineCompletionResult {
        if self.connection_cancel.is_cancelled() {
            return InlineCompletionResult::default();
        }
        let generation = self.routing_generation();
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
        let root = self.resolve_document_root(&params.uri).await;
        if self.connection_cancel.is_cancelled() || self.routing_generation() != generation {
            return InlineCompletionResult::default();
        }
        let feed = root.as_deref().and_then(|r| self.feed_for_workspace(r));
        let editor_snapshot = feed.as_ref().map(|f| f.snapshot_for_editor(self.editor_id));
        let target = editor_snapshot.as_ref().and_then(|s| s.target.clone());

        let context = if let Some(ref r) = root {
            let reflex_ctx = self.reflex_context_for_workspace(r).await;
            if self.connection_cancel.is_cancelled() {
                return InlineCompletionResult::default();
            }
            if let Some(text) = reflex_ctx.text_for_target(&target).await {
                text
            } else {
                self.intent_for_workspace(Some(r)).await.0
            }
        } else {
            String::new()
        };
        if self.connection_cancel.is_cancelled() {
            return InlineCompletionResult::default();
        }
        self.refresh_reflex_context(&params.uri, Some(params.line))
            .await;
        if self.connection_cancel.is_cancelled() {
            return InlineCompletionResult::default();
        }
        let started = Instant::now();
        match reflex
            .suggest_with_cancel(
                &params.uri,
                &text,
                params.version,
                pos,
                &context,
                &language_id,
                Some(&self.connection_cancel),
            )
            .await
        {
            Ok(suggestion) => {
                if self.connection_cancel.is_cancelled() || self.routing_generation() != generation
                {
                    return InlineCompletionResult::default();
                }
                if let Some(ref r) = root {
                    if !self.has_root(r).await {
                        return InlineCompletionResult::default();
                    }
                }
                if let Some(target) = &target {
                    if let Some(feed) = &feed {
                        if !feed.is_incarnation_available(target.session_id, target.incarnation) {
                            return InlineCompletionResult::default();
                        }
                    }
                }
                if self.connection_cancel.is_cancelled() || self.routing_generation() != generation
                {
                    return InlineCompletionResult::default();
                }
                self.log_reflex_call(started.elapsed().as_millis());
                to_result(&suggestion)
            }
            Err(outcome) => {
                // A debounced or out-of-range request never reached the
                // model: logging it would bill a call that did not happen.
                if outcome.called_model()
                    && !self.connection_cancel.is_cancelled()
                    && self.routing_generation() == generation
                {
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

    /// Starts a background refresh of the reflex context when it is due.
    pub(super) async fn refresh_reflex_context(
        self: &Arc<Self>,
        uri: &str,
        cursor_line: Option<u32>,
    ) {
        if self.connection_cancel.is_cancelled() {
            return;
        }
        let Some(root) = self.resolve_document_root(uri).await else {
            return;
        };
        let reflex_context = self.reflex_context_for_workspace(&root).await;
        if !reflex_context.is_enabled() {
            return;
        }
        let (intent, _) = self.intent_for_workspace(Some(&root)).await;
        let feed = self.feed_for_workspace(&root);
        let editor_snapshot = feed.as_ref().map(|f| f.snapshot_for_editor(self.editor_id));
        let target = editor_snapshot.as_ref().and_then(|s| s.target.clone());
        let conversation = editor_snapshot
            .as_ref()
            .map(|s| s.digest.clone())
            .unwrap_or_default();
        let Some(claim) = reflex_context
            .claim(uri, cursor_line, &intent, &conversation, target.as_ref())
            .await
        else {
            return;
        };
        let document = self.documents.lock().await.text_and_language(uri);
        let Some((text, language_id)) = document else {
            reflex_context.release().await;
            return;
        };
        reflex_context.spawn_refresh(
            claim,
            uri,
            intent,
            conversation,
            language_id,
            &text,
            Some(self.connection_cancel.clone()),
        );
    }
}
