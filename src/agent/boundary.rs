use std::sync::Arc;

use crate::provider::openai::ChatMessage;

/// A later delivery failure must not discard an already acknowledged prefix.
#[derive(Debug)]
pub struct BoundaryError {
    pub message: String,
    pub committed: Option<Vec<ChatMessage>>,
}
impl From<String> for BoundaryError {
    fn from(message: String) -> Self {
        Self {
            message,
            committed: None,
        }
    }
}

/// Input is installed only before a provider request, with the previous
/// tool batch settled and no human interaction pending.
pub type SafeBoundary = Arc<
    dyn Fn(
            Vec<ChatMessage>,
            tokio_util::sync::CancellationToken,
        )
            -> futures::future::BoxFuture<'static, Result<Option<Vec<ChatMessage>>, BoundaryError>>
        + Send
        + Sync,
>;
