//! Background child supervision, independent of transport and the agent loop.
mod budget;
mod decision;
pub mod facts;
pub(crate) mod live_result;
mod outcomes;
pub mod root_watchdog;
pub(crate) mod supervisor;
pub mod watchdog;
