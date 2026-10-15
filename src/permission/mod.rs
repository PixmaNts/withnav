//! Permission rules, the fixed floor and the decision path that replace per-call review.
//! See docs/adr/0015-permission-rules-and-instinct-flow.md.

pub mod decide;
pub mod decisions;
pub mod floor;
pub mod rules;
pub mod shell;
pub mod store;
pub mod validation;
