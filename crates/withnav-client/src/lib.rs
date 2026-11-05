//! HTTP and SSE client of the WithNav session server.
//!
//! [`client::Client`] makes the requests; [`stream::subscribe`] follows a session's event stream,
//! reconnecting by itself and resuming after the last event it delivered. Nothing here knows about
//! a terminal or about the server's internals: the types on the wire come from
//! `withnav-client-api`.

pub mod client;
pub mod stream;
