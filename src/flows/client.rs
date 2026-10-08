//! Pins the configured instinct assignment and exposes only canonical identity.
use instinct_flow::{protocol::DecisionRequest, *};
use std::{path::Path, sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;

/// Initialization failure must not uninstall deterministic rules or their tool hook.
pub struct UnavailableClient;
impl DecisionClient for UnavailableClient {
    fn capabilities(&self) -> Capabilities {
        Capabilities { images: false }
    }
    fn identity(&self) -> ClientIdentity {
        ClientIdentity {
            model: "unavailable".into(),
            variant: None,
        }
    }
    fn decide<'a>(
        &'a self,
        _: DecisionRequest<'a>,
        _: &'a CancellationToken,
        _: Instant,
    ) -> BoxFuture<'a, Result<instinct_flow::protocol::DecisionResponse, DecisionError>> {
        Box::pin(async { Err(DecisionError::ProviderError) })
    }
}

pub struct NavaiDecisionClient {
    client: instinct_flow::SystemOneClient,
    identity: ClientIdentity,
}
impl NavaiDecisionClient {
    pub fn new(
        models: &crate::provider::config::ModelsConfig,
        config: &Path,
        _cache: Arc<crate::provider::openai::ClientCache>,
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
        let key = crate::provider::auth::resolve_key(
            config,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )?;
        let client = instinct_flow::SystemOneClient::new(
            format!(
                "{}/systemone",
                resolved.provider.base_url.trim_end_matches('/')
            )
            .parse()?,
            resolved.model.model.clone(),
            key,
            crate::provider::systemone::SYSTEMONE_DEADLINE,
            resolved.provider.headers.clone().into_iter().collect(),
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
            // Admission is shared by every host instance, and consumes the run budget.
            static CALLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);
            let _permit = tokio::select! { biased;
                _=cancel.cancelled()=>return Err(DecisionError::Cancelled),
                _=tokio::time::sleep_until(deadline.into())=>return Err(DecisionError::Timeout),
                permit=CALLS.acquire()=>permit.map_err(|_|DecisionError::ProviderError)?,
            };
            self.client.decide(request, cancel, deadline).await
        })
    }
}
