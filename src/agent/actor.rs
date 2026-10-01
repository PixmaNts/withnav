use tokio_util::sync::CancellationToken;

use crate::provider::config::{ProviderConfig, ReasoningHistory};
use crate::provider::openai::ChatMessage;
use crate::tools::{Registry, ToolContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Running,
    Yielded,
    Stopped,
}

pub struct AgentActor {
    pub transcript: Vec<ChatMessage>,
    pub registry: Registry,
    pub tool_context: ToolContext,
    pub provider: ProviderConfig,
    pub api_key: String,
    pub model: String,
    pub reasoning_history: ReasoningHistory,
    pub max_tokens: Option<u64>,
    /// An optional role prompt, sent as a second system message right after
    /// the system contract on every request. It is not part of the transcript.
    pub system_prompt: Option<String>,
    pub cancel: CancellationToken,
    pub state: AgentState,
    pub pending_ask_id: Option<String>,
}

impl AgentActor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Registry,
        tool_context: ToolContext,
        provider: ProviderConfig,
        api_key: String,
        model: String,
        reasoning_history: ReasoningHistory,
        max_tokens: Option<u64>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            transcript: Vec::new(),
            registry,
            tool_context,
            provider,
            api_key,
            model,
            reasoning_history,
            max_tokens,
            system_prompt: None,
            cancel,
            state: AgentState::Idle,
            pending_ask_id: None,
        }
    }

    /// Sets the role prompt sent after the system contract.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        let text = text.into();
        if let Some(ask_id) = self.pending_ask_id.take() {
            self.transcript.push(ChatMessage::Tool {
                tool_call_id: ask_id,
                content: text,
            });
        } else {
            self.transcript.push(ChatMessage::User { content: text });
        }
    }
}
