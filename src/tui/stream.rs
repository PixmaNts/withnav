//! SSE decoding and cancellation; no UI state mutation.
use super::protocol::ClientEvent;
use futures::Stream;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;
pub type EventStream = Pin<Box<dyn Stream<Item = ClientEvent> + Send>>;
pub fn subscribe(
    _client: super::client::Client,
    _session_id: String,
    _cursor: Option<u64>,
    _generation: super::protocol::Generation,
    _cancel: CancellationToken,
) -> EventStream {
    Box::pin(futures::stream::empty())
}
