//! Session ownership: the registry and router (`service`), what it needs
//! to run (`config`), listing and deletion (`catalog`), teardown and
//! tombstones (`lifecycle`), command acceptance (`message`), the host's posting handle (`sender`), subscriptions
//! (`subscription`), the host's profile policy and
//! lifecycle callbacks (`hooks`), actor construction (`actor`) and the SQLite
//! history (`store`). Transport-free: HTTP and SSE live in `server/`.
mod actor;
mod catalog;
mod config;
mod hooks;
mod lifecycle;
mod message;
mod sender;
mod service;
pub mod store;
mod subscription;

pub use catalog::SessionListing;
pub use config::SessionConfig;
pub use hooks::{NoHooks, SessionHooks, StartedSession};
pub use message::MessageAccepted;
pub use sender::SessionSender;
pub use service::{Service, SessionId, SessionSummary};
pub use subscription::Subscription;
