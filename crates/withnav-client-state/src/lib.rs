//! What a client keeps of one conversation, rebuilt from the session event stream: the
//! transcript blocks, the activity, the model selection, the agents the conversation started
//! and the bookkeeping that matches a client's own requests with the events that answer them.
//!
//! No I/O, no runtime and no terminal code: a client feeds [`apply_envelope`] the decoded events
//! and reads the state back.

pub mod children;
pub mod command;
pub mod conversation;
pub mod envelope;
pub mod projection;
pub mod text;

pub use children::{ChildAgent, ChildStatus};
pub use command::{outcome_text, tokens};
pub use conversation::{
    Activity, Block, BlockKind, Conversation, PermissionPrompt, QuestionPrompt, SkillScript, block,
};
pub use envelope::{EnvelopeContext, apply_envelope};
pub use projection::{
    DEFAULT_AGENT, ModelSwitch, SessionState, apply_selection, clear_answered, keep_bypass,
    keep_newer_context, keep_newer_mcp, model_answered, restore_context, set_variant_offer,
    settle_model_op, take_context,
};
pub use text::strip_controls;

/// An event the state cannot take: an unknown schema or a payload that does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("protocol error: {0}")]
pub struct ProtocolError(pub String);
