//! Phase 2 local HTTP server (axum): loopback-only bind, session management,
//! SSE/WS event streaming, and command routing. Implemented in todos 6-9.
pub mod routes;
pub mod session;
pub mod sse;
pub mod websocket;
