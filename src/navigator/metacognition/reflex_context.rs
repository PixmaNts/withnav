//! The reflex-context producer: the `ReflexContextModel` impl of the commenter,
//! one short plain chat call with its prompt and caps.

use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::LlmCommenter;
use crate::navigator::ports::ModelError;

/// The system prompt for the reflex-context refresh.
const REFLEX_CONTEXT_PROMPT: &str = include_str!("reflex_context_prompt.md");
/// The refresh is capped well under the remark budget's tool loop: one
/// short plain call.
const REFLEX_CONTEXT_BUDGET: Duration = Duration::from_secs(25);
const REFLEX_CONTEXT_MAX_TOKENS: u64 = 300;

impl crate::navigator::ports::ReflexContextModel for LlmCommenter {
    fn refresh<'a>(
        &'a self,
        request: &'a crate::navigator::ports::ReflexContextRequest,
    ) -> BoxFuture<'a, Result<String, ModelError>> {
        use crate::provider::client::ModelClient;
        use crate::provider::openai::{ChatMessage, ChatRequest, StreamOptions};
        use crate::provider::{StreamDelta, start_and_consume};

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
                    ChatMessage::User {
                        provenance: Default::default(),
                        content: user,
                        images: Vec::new(),
                    },
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
                extra: self.variant.clone().unwrap_or_default(),
                images: Default::default(),
            };
            let client = ModelClient::new(
                &self.resolved.provider_name,
                &self.resolved.provider,
                self.api_key.clone(),
                &self.clients,
            )?;
            let cancel = CancellationToken::new();
            let call = async {
                let http = client.prepare_request(client.post("chat/completions").json(&chat));
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
