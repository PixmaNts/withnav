//! The metacognition layer's model, as the service's own bounded checks call it: the goal
//! completion check and the parallel permission review. One construction for both, so a change
//! to how the layer's key, client or variant is resolved reaches every caller.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::service::Service;
use crate::provider::assignments::{AssignedModel, Layer};
use crate::provider::client::ModelClient;
use crate::provider::openai::{
    ChatMessage, ChatRequest, ClientCache, StreamOptions, UserProvenance,
};

/// The metacognition model, ready for one tool-free chat call at a time.
pub(crate) struct MetacognitionClient {
    client: ModelClient,
    pub(crate) assigned: AssignedModel,
    key: String,
    cache: Arc<ClientCache>,
    catalog: crate::provider::catalog::ModelCatalog,
}

/// The answer of one call: its text and, when the provider reported them, its token counts.
pub(crate) struct Answer {
    pub(crate) text: String,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
}

impl MetacognitionClient {
    /// The layer's model name, as the shadow logs record it.
    pub(crate) fn model(&self) -> String {
        self.assigned.name.to_string()
    }

    /// One tool-free call: `system` instructions, then `input` as navigator data, at most
    /// `max_tokens` of answer. `None` on any provider failure; never retried.
    pub(crate) async fn ask(
        &self,
        system: &str,
        input: String,
        max_tokens: u64,
        cancel: &CancellationToken,
    ) -> Option<Answer> {
        let chat = crate::provider::catalog::resolve_chat(
            &self.catalog,
            &self.assigned.resolved,
            &self.key,
            &self.cache,
        )
        .await
        .ok()?;
        let variant = crate::provider::layers::chat_variant(&self.assigned, &chat).ok()?;
        let request = ChatRequest {
            model: self.assigned.resolved.model.model.clone(),
            messages: vec![
                ChatMessage::System {
                    content: system.to_owned(),
                },
                ChatMessage::User {
                    provenance: UserProvenance::Navigator,
                    content: input,
                    images: Vec::new(),
                },
            ],
            tools: Vec::new(),
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            max_tokens: Some(max_tokens),
            temperature: None,
            extra: Default::default(),
            images: Default::default(),
        };
        let answer = self
            .client
            .chat(request, variant.as_ref(), cancel, &mut |_| {})
            .await
            .ok()?;
        Some(Answer {
            text: answer.text,
            input_tokens: answer.usage.map(|u| u.prompt_tokens),
            output_tokens: answer.usage.map(|u| u.completion_tokens),
        })
    }
}

impl Service {
    /// The metacognition client, or `None` (silently: the layer is optional) when the layer is
    /// unassigned or disabled, or its key or client is unavailable.
    ///
    /// The same construction as the root watchdog reviewer's: the layer assignment, the
    /// provider key, one `ModelClient` over the service's client cache. The variant is resolved
    /// per call, against the runtime offer, so a model that offers `low` gets it.
    pub(crate) fn metacognition_client(
        &self,
        workspace: &std::path::Path,
    ) -> Option<MetacognitionClient> {
        let (models, _) = crate::provider::config::load_models(
            self.config.models_path.as_deref(),
            workspace,
            &self.config.config_dir,
        )
        .ok()?;
        let assigned = crate::provider::layers::assigned(
            &models,
            &self.config.config_dir,
            Layer::Metacognition,
        )
        .ok()?;
        let resolved = &assigned.resolved;
        let key = crate::provider::auth::resolve_key(
            &self.config.config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )
        .ok()?;
        let client = ModelClient::new(
            &resolved.provider_name,
            &resolved.provider,
            key.clone(),
            &self.client_cache,
        )
        .ok()?;
        Some(MetacognitionClient {
            client,
            assigned,
            key,
            cache: self.client_cache.clone(),
            catalog: self.model_catalog().clone(),
        })
    }
}
