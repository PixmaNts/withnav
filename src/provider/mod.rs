//! The model access layer lives in the `withnav-provider` crate; this module keeps its old paths
//! and adds the two pieces that read `config.json`: which model each layer and agent uses.

pub use crate::{assignments, layers};
pub use withnav_provider::*;
