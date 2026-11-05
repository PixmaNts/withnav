//! The parts of a permission question that clients see on the wire: where a held call acts, and
//! the longest refusal message the server accepts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Maximum Captain-provided refusal reason, measured on the UTF-8 wire.
pub const REJECT_MESSAGE_MAX_BYTES: usize = 2048;
pub const REJECT_MESSAGE_LIMIT_ERROR: &str = "Reject message must be at most 2048 UTF-8 bytes";

/// Where a held call acts, shown to the Captain on the request. It is
/// displayed, never stored as a grant: what an answer saves is a rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Scope {
    /// The canonical folder the call acts in.
    Folder(PathBuf),
    /// A URL origin: `scheme://host[:port]`.
    Origin(String),
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Folder(path) => write!(f, "{}", path.display()),
            Self::Origin(origin) => f.write_str(origin),
        }
    }
}
