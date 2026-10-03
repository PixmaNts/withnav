use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent::definitions::AgentDefinition;
use crate::agent::instructions::Instructions;
use crate::agent::skills::{SkillTool, Skills};
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
    /// The variant merged into every chat request of the next run; `None`
    /// sends the request as built. The session runtime sets it at each turn
    /// start.
    pub variant: Option<crate::provider::config::Variant>,
    /// Model steps one run may take before it yields; read from the
    /// environment once, when the actor is built.
    pub max_steps: usize,
    /// An optional role prompt, sent as a second system message right after
    /// the system contract on every request. It is not part of the transcript.
    pub system_prompt: Option<String>,
    /// The primary agent this turn runs as: a snapshot installed when the
    /// turn starts, so a role change never reaches a turn in flight. `None`
    /// for an actor built outside a session. Its prompt is sent together with
    /// `system_prompt`; it is not part of the transcript.
    pub role: Option<Arc<AgentDefinition>>,
    /// The local instructions read when the session was created or resumed,
    /// sent as one system message after the role prompt. A snapshot: the
    /// files are not read again.
    pub rules: Instructions,
    /// The skills found when the session was created or resumed: the catalog
    /// message follows the rules. A snapshot, like the rules.
    pub skills: Arc<Skills>,
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
            variant: None,
            max_steps: crate::agent::run_loop::max_steps(),
            system_prompt: None,
            role: None,
            rules: Instructions::default(),
            skills: Arc::new(Skills::default()),
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

    /// The text of the role system message: the selected agent's prompt, then
    /// the profile's instructions (the profile has the last word). `None`
    /// when both are empty, which is the case for the default `build` agent
    /// on a session without a profile.
    pub fn role_message(&self) -> Option<String> {
        let agent = self
            .role
            .as_ref()
            .map(|role| role.prompt.trim())
            .filter(|prompt| !prompt.is_empty());
        let profile = self
            .system_prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty());
        match (agent, profile) {
            (Some(agent), Some(profile)) => Some(format!("{agent}\n\n{profile}")),
            (Some(only), None) | (None, Some(only)) => Some(only.to_string()),
            (None, None) => None,
        }
    }

    /// Installs the skills snapshot: with at least one skill, the `skill` tool
    /// joins the registry and `read`/`grep` may open the skill roots. Writes
    /// keep their roots.
    pub fn set_skills(&mut self, skills: Skills) {
        let skills = Arc::new(skills);
        if !skills.is_empty() {
            self.registry
                .register(Box::new(SkillTool::new(Arc::clone(&skills))));
        }
        self.tool_context.readable_roots = skills.readable_roots().to_vec();
        self.skills = skills;
    }

    /// The estimated size of the request this actor would send now, by
    /// category (see [`crate::agent::context`]).
    pub fn context_breakdown(&self) -> crate::agent::context::Breakdown {
        // The same role message `build_request` sends: the selected agent's
        // prompt composed with the profile's.
        let role = self.role_message();
        crate::agent::context::estimate(
            role.as_deref(),
            self.rules.message(),
            self.skills.catalog(),
            &self.transcript,
            &self.registry.schemas(),
        )
    }

    /// Re-derives `pending_ask_id` from a restored transcript: the `ask` call
    /// of the last assistant message that has no `Tool` message after it. The
    /// transcript is used (not the stored `agent.yield` event) because that
    /// event does not carry the tool call id, and a dangling `ask` needs its
    /// result whatever the events say.
    pub fn restore_pending_ask(&mut self) {
        self.pending_ask_id = None;
        let Some(last) = self
            .transcript
            .iter()
            .rposition(|m| matches!(m, ChatMessage::Assistant { .. }))
        else {
            return;
        };
        let ChatMessage::Assistant { tool_calls, .. } = &self.transcript[last] else {
            return;
        };
        let answered = |id: &str| {
            self.transcript[last + 1..]
                .iter()
                .any(|m| matches!(m, ChatMessage::Tool { tool_call_id, .. } if tool_call_id == id))
        };
        self.pending_ask_id = tool_calls
            .iter()
            .rev()
            .find(|tc| tc.function.name == "ask" && !answered(&tc.id))
            .map(|tc| tc.id.clone());
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
