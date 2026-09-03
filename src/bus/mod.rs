//! Phase 2 event bus: per-session sequencer, event envelopes, per-actor
//! inboxes, and the agent runtime task. Implemented in todos 2-5.
pub mod command;
pub mod event;
pub mod inbox;
pub mod runtime;
pub mod sequencer;
