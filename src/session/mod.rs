//! Session ownership: the registry and router (`service`), teardown and
//! tombstones (`lifecycle`), command acceptance (`message`), subscriptions
//! (`subscription`), actor construction (`actor`) and the SQLite
//! history (`store`). Transport-free: HTTP and SSE live in `server/`.
mod actor;
pub mod lifecycle;
mod message;
pub mod service;
pub mod store;
mod subscription;

pub use actor::{COGNITION_PROMPT, NAVIGATOR_PROFILE};
pub use lifecycle::{Tombstone, MAX_TOMBSTONES};
pub use message::MessageAccepted;
pub use service::{
    Lifecycle, ServerConfig, Service, SessionHandle, SessionId, SessionSlot, SessionSummary,
};
pub use subscription::Subscription;
