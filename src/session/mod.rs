//! Session ownership: the registry and router (`service`), teardown and
//! tombstones (`lifecycle`), actor construction (`actor`) and the SQLite
//! history (`store`). Transport-free: HTTP and SSE live in `server/`.
mod actor;
pub mod lifecycle;
pub mod service;
pub mod store;

pub use actor::{COGNITION_PROMPT, NAVIGATOR_PROFILE};
pub use lifecycle::{Tombstone, MAX_TOMBSTONES};
pub use service::{
    Lifecycle, ServerConfig, Service, SessionHandle, SessionId, SessionSlot, SessionSummary,
};
