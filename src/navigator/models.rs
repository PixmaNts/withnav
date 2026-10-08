//! Adapters putting the provider clients behind the navigator's model seams.
//! A provider failure stays [`ModelError::Provider`]; a response that is not
//! the expected shape becomes [`ModelError::Malformed`].

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::navigator::ports::{FimAnswer, FimModel, FimRequest, ModelError};
use crate::provider::fim::InceptionFim;

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
