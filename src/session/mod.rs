//! Session ownership: the registry and router (`service`), what it needs
//! to run (`config`), listing and deletion (`catalog`), teardown and close
//! (`lifecycle`), command acceptance (`message`), the host's posting handle
//! (`sender`), subscriptions (`subscription`), the host's profile policy and
//! lifecycle callbacks (`hooks`), the instructions log (`rules_log`), actor construction (`actor`) and the SQLite
//! history (`store`). Transport-free: HTTP and SSE live in `server/`.
mod actor;
mod catalog;
mod commands;
mod config;
mod guard;
mod hooks;
mod lifecycle;
mod message;
mod rules_log;
mod run_command;
mod sender;
mod service;
#[cfg(debug_assertions)]
#[doc(hidden)]
pub use service::ChildPreparationBarrier;
pub mod store;
mod subscription;

pub use catalog::SessionListing;
pub use commands::{route_line, CommandOutcome, CommandSpec, Routed, COMMANDS};
pub use config::SessionConfig;
pub use hooks::{NoHooks, SessionHooks, StartedSession};
pub use message::MessageAccepted;
pub use sender::SessionSender;
pub use service::{Service, SessionId, SessionSummary, VariantChoice, CLEAR_WORD};
pub use subscription::Subscription;
