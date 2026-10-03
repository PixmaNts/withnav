use std::sync::Arc;

use crate::agent::actor::AgentActor;
use crate::provider::openai::ChatMessage;

pub type Delivery = Arc<
    dyn Fn(Vec<ChatMessage>) -> super::BoxFuture<Result<Option<Vec<ChatMessage>>, String>>
        + Send
        + Sync,
>;

pub(super) struct DeliveryCell(pub Delivery);
impl std::fmt::Debug for DeliveryCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Delivery")
    }
}

pub(super) async fn apply(
    actor: &mut AgentActor,
    status: &super::RuntimeStatus,
) -> Result<bool, String> {
    if actor.has_pending() {
        return Ok(false);
    }
    let callback = status
        .delivery
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|cell| cell.0.clone());
    let Some(callback) = callback else {
        return Ok(false);
    };
    if let Some(transcript) = callback(actor.transcript.clone()).await? {
        actor.transcript = transcript;
        Ok(true)
    } else {
        Ok(false)
    }
}
