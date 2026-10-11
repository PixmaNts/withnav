use std::sync::Arc;

use crate::provider::openai::ChatMessage;

/// Only changed histories replace the actor's complete settled transcript.
pub enum BoundaryResult {
    Unchanged,
    Continue(Vec<ChatMessage>),
    Interrupted,
    Watchdog(Option<Vec<ChatMessage>>),
}

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
    dyn for<'a> Fn(
            &'a [ChatMessage],
            tokio_util::sync::CancellationToken,
        ) -> futures::future::BoxFuture<'a, Result<BoundaryResult, BoundaryError>>
        + Send
        + Sync,
>;
