//! Stands in for the `withnav-core` crate: ids, domain messages and the ports the engine injects.
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::Value;
use std::time::Duration;

macro_rules! id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(pub String);
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}
id!(MessageId);
id!(ToolCallId);
id!(ToolName);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Who produced a message. A plain enum: any code can write `Origin::Captain`, unlike the design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Captain,
    Parent,
    Subagent,
    Navigator,
    Host,
}

/// Provider-neutral transcript unit (images, thinking and skill blocks are left out of the sketch).
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub origin: Origin,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text(String),
    ToolCall(ToolCall),
    ToolResult { call: ToolCallId, output: ToolOutput },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolArguments(pub Value);

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: ToolName,
    pub args: ToolArguments,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Error,
    Denied,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub status: ToolStatus,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    Read,
    Write,
}

/// How much the Captain delegated for this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Normal,
    Bypass,
    Yolo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
}

/// A stream that ends without `Done` is an error; the partial output is kept by the caller.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    Text(String),
    ToolCallStarted { index: u32, id: ToolCallId, name: ToolName },
    ToolCallArgs { index: u32, fragment: String },
    Done { finish: FinishReason },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("the model stream ended without Done")]
    Truncated,
    #[error("model protocol error: {0}")]
    Protocol(String),
}

#[derive(Debug, thiserror::Error)]
#[error("tool failed: {0}")]
pub struct ToolError(pub String);

pub trait ModelProvider: Send + Sync {
    fn chat<'a>(
        &'a self,
        req: ChatRequest,
    ) -> BoxFuture<'a, Result<BoxStream<'a, Result<ModelEvent, ModelError>>, ModelError>>;
}

pub struct ToolDescriptor {
    pub name: ToolName,
    pub class: ToolClass,
}

pub trait Tool: Send + Sync {
    fn descriptor(&self) -> &ToolDescriptor;
    fn execute<'a>(&'a self, args: ToolArguments) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;
}

/// The processing chains a stage can sit in. The review slot belongs to `ToolAuthorize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    ToolPrepare,
    ToolAuthorize,
    ToolResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    Continued,
    Halted,
    Failed,
}

/// Evidence of one stage run. Lives in core because the `Telemetry` port consumes it.
#[derive(Debug, Clone, PartialEq)]
pub struct StageObservation {
    pub stage: String,
    pub version: u32,
    pub chain: Chain,
    pub duration: Duration,
    pub outcome: StageOutcome,
    pub call: Option<ToolCallId>,
}

/// Best effort and synchronous: telemetry never blocks or fails an operation.
pub trait Telemetry: Send + Sync {
    fn record(&self, observation: StageObservation);
}
