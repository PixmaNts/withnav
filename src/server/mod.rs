//! Local HTTP server (axum): loopback-only bind, session routes, SSE event
//! streaming and command routing. Session ownership lives in
//! [`crate::session`]; this module is the HTTP adapter over it.
//!
//! Surface: [`routes::app`] (health + session create/get), [`AppState`]
//! (the HTTP state wrapping the session service), and [`routes::serve`]
//! (loopback bind with graceful shutdown).
pub mod routes;
pub mod session;
pub mod sse;

// Compatibility path: remove once callers import from `crate::session::store`.
pub use crate::session::store;

pub use routes::{app, serve};
pub use session::{AppState, ServerConfig};
