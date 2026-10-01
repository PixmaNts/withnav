//! Phase 2 local HTTP server (axum): loopback-only bind, session management,
//! SSE event streaming, and command routing. Implemented in todos 6-9.
//!
//! Todo 6 surface: [`routes::app`] (health + session create/get), the
//! [`session`] manager (registry + router), and [`routes::serve`] (loopback
//! bind with graceful shutdown). Re-exports the pieces Todo 10's `serve`
//! wiring and todos 7-9 consume.
pub mod routes;
pub mod session;
pub mod sse;
pub mod store;

pub use routes::{app, serve};
pub use session::{AppState, ServerConfig};
