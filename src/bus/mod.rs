//! Phase 2 event bus: per-session sequencer, event envelopes, per-actor
//! inboxes, and the agent runtime task. Implemented in todos 2-5.
//!
//! Todo 2 delivered `event` (envelope + 12-kind catalog) and `sequencer`
//! (single authoritative seq-assignment task). `inbox`/`command` (todo 4),
//! `runtime` (todo 5), and the ring + fan-out living inside `sequencer`
//! (todo 3) are still stubs.
pub mod command;
pub mod event;
pub mod inbox;
pub mod runtime;
pub mod sequencer;
