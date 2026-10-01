//! Phase 2 local HTTP server (axum): loopback-only bind, session management,
//! SSE event streaming, and command routing.
//!
//! Surface: [`routes::app`] (health + session create/get), the
//! [`session`] manager (registry + router), and [`routes::serve`] (loopback
//! bind with graceful shutdown).
pub mod routes;
pub mod session;
pub mod sse;
pub mod store;

pub use routes::{app, serve};
pub use session::{AppState, ServerConfig};
