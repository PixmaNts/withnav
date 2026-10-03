use std::sync::Arc;

use crate::provider::openai::ChatMessage;

/// Input is installed only before a provider request, with the previous
/// tool batch settled and no human interaction pending.
pub type SafeBoundary = Arc<
    dyn Fn(
            Vec<ChatMessage>,
        ) -> futures::future::BoxFuture<'static, Result<Option<Vec<ChatMessage>>, String>>
        + Send
        + Sync,
>;
