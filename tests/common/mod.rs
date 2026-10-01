//! Test support shared by the integration tests. Each `tests/*.rs` file is
//! its own crate and pulls this in with `mod common;`, so a helper one file
//! does not use is dead code there: hence the crate-level allow.
#![allow(dead_code)]

pub mod navigator;
pub mod server;
pub mod sse;
