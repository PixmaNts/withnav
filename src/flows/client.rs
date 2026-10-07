//! Pins the configured instinct assignment and exposes only canonical identity.
use instinct_flow::{protocol::DecisionRequest, *};
use std::{path::Path, sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;

pub struct NavaiDecisionClient {
    client: crate::provider::systemone::SystemOneClient,
    identity: ClientIdentity,
}
impl NavaiDecisionClient {
    pub fn new(
        models: &crate::provider::config::ModelsConfig,
        config: &Path,
        cache: Arc<crate::provider::openai::ClientCache>,
    ) -> anyhow::Result<Self> {
        let assignment = crate::provider::layers::assigned(
            models,
            config,
            crate::provider::assignments::Layer::Instinct,
        )?;
        let identity = ClientIdentity {
            model: assignment.name.to_string(),
            variant: assignment.variant.clone(),
        };
        let resolved = assignment.resolved;
        let client = crate::provider::systemone::SystemOneClient::new(
            &resolved.provider_name,
            &resolved.provider,
            &resolved.model,
            config,
            cache,
        )?;
        Ok(Self { client, identity })
    }
}
impl DecisionClient for NavaiDecisionClient {
    fn capabilities(&self) -> Capabilities {
        Capabilities { images: false }
    }
    fn identity(&self) -> ClientIdentity {
        self.identity.clone()
    }
    fn decide<'a>(
        &'a self,
        request: DecisionRequest<'a>,
        cancel: &'a CancellationToken,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>> {
        Box::pin(async move {
            let response = tokio::select! { biased;
                _=cancel.cancelled()=>return Err(DecisionError::Cancelled),
                _=tokio::time::sleep_until(deadline.into())=>return Err(DecisionError::Timeout),
                response=self.client.decide(&request,cancel)=>response.map_err(|_|DecisionError::ProviderError)?,
            };
            if cancel.is_cancelled() {
                return Err(DecisionError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(DecisionError::Timeout);
            }
            instinct_flow::client::validate_answers(request.questions, &response.answers)?;
            Ok(response)
        })
    }
}
