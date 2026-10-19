//! The wire contract between the WithNav session server and its clients: the event envelope and
//! every payload it carries, the request and response bodies of the HTTP API, and the small
//! value types those refer to.
//!
//! Data only. Nothing here runs a task, opens a socket or draws a screen, so a client can depend
//! on this crate without pulling in the server's runtime.

pub mod attachment;
pub mod command;
pub mod event;
pub mod permissions;
pub mod protocol;
pub mod provider;
pub mod settings;
pub mod tools;
