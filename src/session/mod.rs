//! Session ownership: the registry and router (`service`), what it needs
//! to run (`config`), listing and deletion (`catalog`), teardown and
//! tombstones (`lifecycle`), command acceptance (`message`), subscriptions
//! (`subscription`), actor construction (`actor`) and the SQLite
//! history (`store`). Transport-free: HTTP and SSE live in `server/`.
mod actor;
mod catalog;
mod config;
mod hooks;
pub mod lifecycle;
mod message;
pub mod service;
pub mod store;
mod subscription;

pub use catalog::SessionListing;
pub use config::SessionConfig;
pub use hooks::{NoHooks, SessionHooks};
pub use lifecycle::{Tombstone, MAX_TOMBSTONES};
pub use message::MessageAccepted;
pub use service::{Lifecycle, Service, SessionHandle, SessionId, SessionSlot, SessionSummary};
pub use subscription::Subscription;
