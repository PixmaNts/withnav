use std::{future::Future, path::PathBuf, pin::Pin};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub enum FlowVerdict {
    NoObjection,
    Deny {
        note: String,
    },
    AskDriver {
        note: String,
        flow: String,
        run_id: String,
    },
}

#[derive(Debug)]
pub struct ToolCallContext {
    pub call_id: String,
    pub tool: String,
    pub args: Value,
    pub agent: Option<String>,
    pub workspace: PathBuf,
}

pub trait ToolCallFlows: Send + Sync + std::fmt::Debug {
    fn pre_call<'a>(
        &'a self,
        call: &'a ToolCallContext,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = FlowVerdict> + Send + 'a>>;
}
