//! Adapters putting the provider clients behind the navigator's model seams.
//! A provider failure stays [`ModelError::Provider`]; a response that is not
//! the expected shape becomes [`ModelError::Malformed`].

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::navigator::ports::{
    Answers, DecisionModel, FimAnswer, FimModel, FimRequest, ModelError, Questions,
};
use crate::provider::fim::InceptionFim;
use crate::provider::systemone::{DecisionRequest, SystemOneClient};

/// The reflex's [`FimModel`] over the Inception fill-in-the-middle client.
pub struct FimAdapter(pub InceptionFim);

impl FimModel for FimAdapter {
    fn complete<'a>(
        &'a self,
        request: FimRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<FimAnswer, ModelError>> {
        Box::pin(async move {
            let done = self
                .0
                .complete(&request.prompt, &request.suffix, request.max_tokens, cancel)
                .await?;
            Ok(FimAnswer {
                text: done.text,
                usage: done.usage,
                latency: done.latency,
            })
        })
    }
}

/// The instinct's [`DecisionModel`] over the System One client.
#[derive(Debug, Clone)]
pub struct DecisionAdapter(pub SystemOneClient);

impl DecisionModel for DecisionAdapter {
    fn decide<'a>(
        &'a self,
        state: &'a str,
        questions: &'a Questions,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Answers, ModelError>> {
        Box::pin(async move {
            let response = self
                .0
                .decide(&DecisionRequest { state, questions }, cancel)
                .await?;
            Ok(response.answers)
        })
    }
}
