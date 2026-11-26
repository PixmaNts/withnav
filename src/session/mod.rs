//! Session ownership: the registry and router (`service`), what it needs
//! to run (`config`), listing and deletion (`catalog`), teardown and close
//! (`lifecycle`), the per-tree lifecycle gates (`gate`), command acceptance (`message`), the host's posting handle
//! (`sender`), subscriptions (`subscription`), the host's editor policy and
//! lifecycle callbacks (`hooks`), the instructions log (`rules_log`), actor construction (`actor`), the SQLite
//! history (`store`), the session ledger folded from it (`ledger`), the Captain's `/goal`
//! (`goal`) and the shadow check of its criteria after each turn (`goal_check`). Transport-free:
//! HTTP and SSE live in `server/`.
mod actor;
pub mod attachments;
mod catalog;
mod commands;
mod config;
mod flows;
mod gate;
mod goal;
pub mod goal_check;
mod hooks;
mod instance;
pub mod ledger;
mod lifecycle;
mod mcp;
pub(crate) mod memory;
mod message;
mod metacognition_client;
pub mod permission_review;
pub mod preflight;
mod prompt_command;
mod rules_log;
mod run_command;
mod secrets;
mod sender;
pub(crate) mod service;
pub(crate) mod test_points;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use test_points::ChildPreparationBarrier;
pub mod store;
pub mod subagents;
mod subscription;

pub use catalog::SessionListing;
pub use commands::{COMMANDS, CommandOutcome, CommandSpec, Routed, route_line};
pub use config::SessionConfig;
pub use hooks::{NoHooks, SessionHooks, StartedSession};
pub use message::MessageAccepted;
pub use sender::SessionSender;
pub use service::{
    AnswerOutcome, CLEAR_WORD, CaptainAnswer, Service, SessionId, SessionMetadata, SkillEntry,
    SkillsBody, VariantChoice,
};
pub use subscription::Subscription;
