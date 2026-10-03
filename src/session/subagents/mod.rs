//! Background child supervision, independent of transport and the agent loop.
mod decision;
mod outcomes;
pub(crate) mod supervisor;
pub mod watchdog;
