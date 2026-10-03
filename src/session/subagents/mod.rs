//! Background child supervision, independent of transport and the agent loop.
mod decision;
pub(crate) mod live_result;
mod outcomes;
pub(crate) mod supervisor;
pub mod watchdog;
