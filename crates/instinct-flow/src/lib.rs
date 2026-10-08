//! Host-independent decision graphs: validated definitions propose, never apply, actions.
mod admission;
pub mod catalogue;
pub mod client;
pub mod definition;
pub mod engine;
pub mod expression;
pub mod protocol;
pub mod record;
pub mod rendering;
pub mod validation;
pub use catalogue::*;
pub use client::*;
pub use definition::*;
pub use engine::*;
pub use record::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub path: String,
    pub node_id: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}
impl Diagnostic {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            path: path.into(),
            node_id: None,
            line: None,
            column: None,
        }
    }
    pub fn at_node(mut self, id: &str) -> Self {
        self.node_id = Some(id.into());
        self
    }
}
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
