use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent::compaction::{
    effective_history, project_reasoning, validate as validate_compaction, Compaction, Components,
};
use crate::agent::definitions::{AgentAccess, AgentDefinition};
use crate::agent::instructions::Instructions;
use crate::agent::permissions::PermissionRequest;
use crate::agent::skills::{SkillTool, Skills};
use crate::provider::config::{ProviderConfig, ReasoningHistory};
use crate::provider::openai::ChatMessage;
use crate::provider::ToolCall;
use crate::tools::{Registry, ToolContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Running,
    Yielded,
    Stopped,
}

/// What a yielded actor waits for: an answer to an `ask`, or a decision on a
/// gated tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingInteraction {
    /// The next user text is the result of the `ask` call `tool_call_id`.
    Ask { tool_call_id: String },
    /// `call` has not run: it waits for a decision on `request`, and the
    /// sibling calls of the same assistant message that follow it wait behind
    /// it. Only a typed decision resolves it, never user text.
    Permission {
        request: Box<PermissionRequest>,
        call: ToolCall,
        remaining_calls: Vec<ToolCall>,
        /// For `write` and `edit`: the canonical file the request was made
        /// for. The call runs on an approval only if it still resolves there.
        destination: Option<std::path::PathBuf>,
        /// The model steps left in the turn when the call was held.
        steps_left: usize,
    },
}

/// Captain messages as permission evidence, and whether any were cut.
pub type CaptainEvidence = (Vec<String>, bool);

/// A child's shared snapshot of its parent's Captain evidence.
pub type ParentCaptain = Arc<std::sync::Mutex<Option<CaptainEvidence>>>;

/// Where the configuration and the database the fixed floor protects live.
#[derive(Debug, Clone, Default)]
pub struct PermissionDirs {
    pub config: Option<std::path::PathBuf>,
    pub data: Option<std::path::PathBuf>,
}

impl PermissionDirs {
    /// This machine's configured directories.
    pub fn machine() -> Self {
        Self {
            config: crate::provider::auth::default_config_dir(),
            data: crate::paths::data_dir(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BypassState {
    pub enabled: bool,
    pub seq: u64,
}

pub struct AgentActor {
    pub compaction_writer: Option<super::compaction::SummaryWriter>,
    pub compaction_catalog: Option<crate::provider::catalog::ModelCatalog>,
    pub compaction_max_message_tokens: Option<u64>,
    pub compaction_chunk_tokens: u64,
    pub compaction_progress: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
    pub tool_flows: Option<Arc<dyn super::flow_hook::ToolCallFlows>>,
    /// The permission rules and the decision table. A session opens the
    /// shared database; an actor built outside one keeps a private, empty one.
    pub permission_store: Option<Arc<crate::permission::store::PermissionStore>>,
    /// The open error when the shared store is unavailable, exposed to clients.
    pub permissions_unavailable: Option<String>,
    /// WithNav's own configuration and data directories, which the fixed floor protects.
    pub permission_dirs: PermissionDirs,
    /// The session the decision table files this actor's decisions under.
    pub permission_session: String,
    /// The turn the decision table files decisions under; renewed with each run.
    pub permission_turn: String,
    /// Identical consecutive calls in this turn.
    pub doom: crate::permission::decide::DoomTracker,
    /// A child's snapshot of its parent's Captain messages: the Captain's
    /// request reaches a delegated call only through it, never through the
    /// delegated prompt or the parent's steering text.
    pub parent_captain: ParentCaptain,
    pub bypass: Arc<std::sync::Mutex<BypassState>>,
    /// Private temporary storage offered to the model; a write there is bound
    /// to the directory's identity (inode and mode).
    pub scratch_roots: Vec<super::scratch::Identity>,
    /// Captain setting `yolo`: no permission control at all for this session.
    pub yolo: bool,
    /// The Instinct judge compaction asks which old tool outputs still matter.
    pub compaction_decision: Option<Arc<dyn crate::provider::systemone::DecisionModel>>,
    pub safe_boundary: Option<super::boundary::SafeBoundary>,
    pub transcript: Vec<ChatMessage>,
    /// The installed compaction: what stands for the start of `transcript`
    /// and which tool outputs the prompt shows as markers. `transcript`
    /// itself stays the full history.
    pub compaction: Option<Compaction>,
    pub registry: Registry,
    pub tool_context: ToolContext,
    pub provider: ProviderConfig,
    pub api_key: String,
    pub model: String,
    pub reasoning_history: ReasoningHistory,
    /// Transcript messages before this index were written by a model the
    /// session no longer talks to: the request projection drops their
    /// reasoning (it is only replayed to the model that produced it). The
    /// transcript itself keeps it.
    pub reasoning_floor: usize,
    pub max_tokens: Option<u64>,
    /// The input budget a request must fit, in tokens, set by the session
    /// runtime at each turn start. With one, a run stops before a request
    /// that would exceed it ([`crate::agent::run_loop::RunOutcome::NeedsCompaction`]);
    /// without one (a run outside a session) requests are never held back.
    pub input_budget: Option<u64>,
    /// The variant merged into every chat request of the next run; `None`
    /// sends the request as built. The session runtime sets it at each turn
    /// start.
    pub variant: Option<crate::provider::config::Variant>,
    /// Model steps one run may take before it yields. Root actors default to
    /// unbounded (`usize::MAX`); session preparation installs the child or
    /// unavailable-instinct fallback budget. An explicit environment cap wins.
    pub max_steps: usize,
    pub watchdog_steps_left: Option<usize>,
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
    pub pending: Option<PendingInteraction>,
    pub selected_skill_snapshots: Vec<(u64, String)>,
    pub skill_turn_generation: u64,
    pub skill_turn_start: usize,
    pub skill_turn_open: bool,
    /// What the Captain last asked for: the text of their latest message, or
    /// their answer to an `ask`. Set only from a command whose source is the
    /// captain (a human ingress identity), never from a navigator wake, delegated
    /// text or a subagent result. The watchdog judges progress against it. Not stored on its own: a resumed session rebuilds it
    /// from the stored `message.received` events (the newest from the Captain),
    /// so it is `None` only when the Captain never spoke.
    pub captain_task: Option<String>,
}

impl AgentActor {
    /// The registry is the frozen session/child ceiling. A role only narrows it;
    /// switching roles never removes entries or restores unavailable tools.
    pub fn tool_allowed(&self, name: &str) -> bool {
        self.tool_allowed_for_role(name, self.role.as_deref())
    }

    /// Destination-role projection for switch preparation, without mutation.
    pub fn tool_allowed_for_role(&self, name: &str, role: Option<&AgentDefinition>) -> bool {
        self.registry.get(name).is_some()
            && role.is_none_or(|role| {
                role.tools.get(name).copied().unwrap_or(true)
                    && (role.access != AgentAccess::ReadOnly
                        || !matches!(
                            name,
                            "write"
                                | "edit"
                                | "bash"
                                | "ask"
                                | "task"
                                | "steer_subagent"
                                | "decide_subagent"
                                | "intent"
                        ))
            })
    }

    /// Read-only follows the known child access exclusions, including session
    /// control and the navigator's intent writer. Unknown installed tools keep
    /// their declared policy; they are not guessed to be mutations by name.
    pub fn effective_tool_schemas(&self) -> Vec<crate::provider::openai::ToolDef> {
        self.effective_tool_schemas_for_role(self.role.as_deref())
    }

    pub fn effective_tool_schemas_for_role(
        &self,
        role: Option<&AgentDefinition>,
    ) -> Vec<crate::provider::openai::ToolDef> {
        self.registry
            .schemas_filtered(|name| self.tool_allowed_for_role(name, role))
    }

    pub fn effective_skills_catalog(&self) -> Option<&str> {
        self.effective_skills_catalog_for_role(self.role.as_deref())
    }

    pub fn effective_skills_catalog_for_role(
        &self,
        role: Option<&AgentDefinition>,
    ) -> Option<&str> {
        self.tool_allowed_for_role("skill", role)
            .then(|| self.skills.catalog())
            .flatten()
    }

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
            compaction_writer: None,
            compaction_catalog: None,
            compaction_max_message_tokens: None,
            compaction_chunk_tokens: 64_000,
            compaction_progress: None,
            tool_flows: None,
            permission_store: Some(Arc::new(
                crate::permission::store::PermissionStore::in_memory()
                    .expect("an in-memory permission database opens"),
            )),
            permissions_unavailable: None,
            permission_dirs: PermissionDirs::machine(),
            permission_session: String::new(),
            permission_turn: String::new(),
            doom: Default::default(),
            parent_captain: Arc::default(),
            scratch_roots: Vec::new(),
            yolo: false,
            bypass: Arc::default(),
            compaction_decision: None,
            transcript: Vec::new(),
            compaction: None,
            registry,
            safe_boundary: None,
            tool_context,
            provider,
            api_key,
            model,
            reasoning_history,
            reasoning_floor: 0,
            max_tokens,
            input_budget: None,
            variant: None,
            max_steps: crate::agent::run_loop::root_max_steps(),
            watchdog_steps_left: None,
            system_prompt: None,
            role: None,
            rules: Instructions::default(),
            skills: Arc::new(Skills::default()),
            cancel,
            state: AgentState::Idle,
            pending: None,
            selected_skill_snapshots: vec![],
            skill_turn_generation: 0,
            skill_turn_start: 0,
            skill_turn_open: false,
            captain_task: None,
        }
    }

    /// Sets the role prompt sent after the system contract.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// The text of the role system message: the selected agent's prompt, then
    /// the editor's instructions (the editor has the last word). `None`
    /// when both are empty, which is the case for the default `build` agent
    /// on a session without editor context.
    pub fn role_message(&self) -> Option<String> {
        let message = compose_role_message(
            self.role.as_ref().map(|role| role.prompt.as_str()),
            self.system_prompt.as_deref(),
        );
        let usable: Vec<_> = self
            .scratch_roots
            .iter()
            .filter(|root| root.valid())
            .collect();
        if usable.is_empty() {
            message
        } else {
            Some(format!(
                "{}\nTemporary scratch roots (private first, recommended): {}",
                message.as_deref().unwrap_or(""),
                usable
                    .iter()
                    .map(|root| root.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
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

    /// The most recently admitted skill in this turn; new Captain input clears it.
    pub fn active_validated_skill(&self) -> Option<(String, String)> {
        self.skill_turn_open
            .then(|| self.skills.active_validated_skill())
            .flatten()
    }

    pub fn begin_skill_turn(&mut self) {
        self.end_skill_turn();
        self.skill_turn_generation = self.skill_turn_generation.wrapping_add(1);
        self.skill_turn_start = self.transcript.len();
        self.skill_turn_open = true;
    }

    pub fn end_skill_turn(&mut self) {
        self.skills.validation.begin_turn();
        self.selected_skill_snapshots.clear();
        self.skill_turn_open = false;
    }

    /// The parts of the request besides the history. `role` is
    /// [`AgentActor::role_message`], which the caller keeps alive.
    pub fn components<'a>(
        &'a self,
        role: Option<&'a str>,
        tools: &'a [crate::provider::openai::ToolDef],
    ) -> Components<'a> {
        Components {
            system_prompt: role,
            rules: self.rules.message(),
            skills: self.effective_skills_catalog(),
            tools,
        }
    }

    /// The history the next request carries: the transcript as it is, or,
    /// with a compaction installed, the labelled summary then the messages
    /// after its boundary with the pruned outputs as markers.
    ///
    /// Reasoning written by an earlier model (before `reasoning_floor`) is
    /// left out, and so is an assistant message that held nothing else.
    pub fn effective_history(&self) -> Vec<ChatMessage> {
        self.effective_history_at(self.reasoning_floor)
    }

    /// [`AgentActor::effective_history`] as it would be with the reasoning
    /// floor at `floor`, without changing the actor: the history a switch to
    /// another model would send.
    pub fn effective_history_at(&self, floor: usize) -> Vec<ChatMessage> {
        effective_history(
            &super::skills::validation::project_skill_history(
                &project_reasoning(&self.transcript, floor),
                if self.skill_turn_open {
                    Some(self.skill_turn_start)
                } else {
                    None
                },
            ),
            self.compaction.as_ref(),
        )
    }

    /// The transcript the compaction machinery plans against: the same
    /// projection the request is built from, index for index the transcript.
    pub fn planning_transcript(&self, floor: usize) -> Vec<ChatMessage> {
        project_reasoning(&self.transcript, floor)
    }

    /// The estimated size of the request this actor would send now, by
    /// category (see [`crate::agent::context`]): the effective prompt.
    pub fn context_breakdown(&self) -> crate::agent::context::Breakdown {
        self.context_breakdown_at(self.reasoning_floor)
    }

    /// [`AgentActor::context_breakdown`] for the request with the reasoning
    /// floor at `floor`.
    pub fn context_breakdown_at(&self, floor: usize) -> crate::agent::context::Breakdown {
        // The same role message `build_request` sends: the selected agent's
        // prompt composed with the editor's.
        let role = self.role_message();
        crate::agent::context::estimate(
            role.as_deref(),
            self.rules.message(),
            self.effective_skills_catalog(),
            &self.effective_history_at(floor),
            &self.effective_tool_schemas(),
        )
    }

    /// Whether the actor waits for an answer or a decision.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The permission request the actor waits on, if any.
    pub fn pending_permission(&self) -> Option<&PermissionRequest> {
        match &self.pending {
            Some(PendingInteraction::Permission { request, .. }) => Some(request),
            _ => None,
        }
    }

    /// Persists `compaction` through `persist`, which is handed the full
    /// transcript and must return only once the write is durable, then
    /// installs it. A failed write installs nothing. The compaction is
    /// validated against the transcript first.
    pub async fn commit_compaction<F, Fut>(
        &mut self,
        compaction: Compaction,
        persist: F,
    ) -> Result<(), String>
    where
        F: FnOnce(Vec<ChatMessage>, Compaction) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        validate_compaction(&compaction, &self.transcript).map_err(|e| e.to_string())?;
        persist(self.transcript.clone(), compaction.clone()).await?;
        self.compaction = Some(compaction);
        Ok(())
    }

    /// Re-derives the pending `ask` from a restored transcript: the `ask` call
    /// of the last assistant message that has no `Tool` message after it. The
    /// transcript is used (not the stored `agent.yield` event) because that
    /// event does not carry the tool call id, and a dangling `ask` needs its
    /// result whatever the events say.
    pub fn restore_pending_ask(&mut self) {
        self.pending = None;
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
        self.pending = tool_calls
            .iter()
            .rev()
            .find(|tc| tc.function.name == "ask" && !answered(&tc.id))
            .map(|tc| PendingInteraction::Ask {
                tool_call_id: tc.id.clone(),
            });
    }

    /// Appends user text. It answers a pending `ask`; it never answers a
    /// pending permission: that one is settled as interrupted (neither run
    /// nor approved) so the transcript stays well formed, and the text
    /// follows as an ordinary message.
    /// For a restored transcript: settles, as interrupted, every call of the
    /// last assistant message that has no result and is not the pending
    /// `ask`. Such a call was held by a permission request, or running, when
    /// the process ended; it is never run again. Returns how many were
    /// settled.
    pub fn settle_unanswered_calls(&mut self) -> usize {
        let Some(last) = self
            .transcript
            .iter()
            .rposition(|m| matches!(m, ChatMessage::Assistant { .. }))
        else {
            return 0;
        };
        let ChatMessage::Assistant { tool_calls, .. } = &self.transcript[last] else {
            return 0;
        };
        let asking = match &self.pending {
            Some(PendingInteraction::Ask { tool_call_id }) => Some(tool_call_id.as_str()),
            _ => None,
        };
        let unanswered: Vec<String> = tool_calls
            .iter()
            .filter(|tc| Some(tc.id.as_str()) != asking)
            .filter(|tc| {
                !self.transcript[last + 1..].iter().any(
                    |m| matches!(m, ChatMessage::Tool { tool_call_id, .. } if *tool_call_id == tc.id),
                )
            })
            .map(|tc| tc.id.clone())
            .collect();
        for id in &unanswered {
            self.transcript.push(ChatMessage::Tool {
                tool_call_id: id.clone(),
                content: String::new(),
                answered_by: None,
            });
        }
        unanswered.len()
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push_user_with_provenance(text, crate::provider::openai::UserProvenance::Captain);
    }

    pub fn push_user_with_provenance(
        &mut self,
        text: impl Into<String>,
        provenance: crate::provider::openai::UserProvenance,
    ) {
        if provenance == crate::provider::openai::UserProvenance::Captain {
            self.begin_skill_turn();
        }
        self.push_user_in_current_turn(text, provenance);
    }

    pub fn push_user_in_current_turn(
        &mut self,
        text: impl Into<String>,
        provenance: crate::provider::openai::UserProvenance,
    ) {
        let text = text.into();
        match self.pending.take() {
            Some(PendingInteraction::Ask { tool_call_id }) => {
                self.transcript.push(ChatMessage::Tool {
                    tool_call_id,
                    content: text,
                    answered_by: Some(provenance),
                });
            }
            Some(PendingInteraction::Permission {
                request,
                call,
                remaining_calls,
                ..
            }) => {
                self.cancel_pending_decision(&request);
                self.settle_interrupted(&call, &remaining_calls);
                self.transcript.push(ChatMessage::User {
                    provenance,
                    content: text,
                });
            }
            None => self.transcript.push(ChatMessage::User {
                provenance,
                content: text,
            }),
        }
    }

    /// Settles every call a pending permission holds as interrupted: none of
    /// them runs. For a stop; a pending `ask` is left as it is.
    pub fn interrupt_pending_permission(&mut self) {
        self.end_skill_turn();
        if let Some(PendingInteraction::Permission {
            request,
            call,
            remaining_calls,
            ..
        }) = self
            .pending
            .take_if(|p| matches!(p, PendingInteraction::Permission { .. }))
        {
            self.cancel_pending_decision(&request);
            self.settle_interrupted(&call, &remaining_calls);
        }
    }

    /// A question nobody answers ends as `cancelled` in the decision table.
    fn cancel_pending_decision(&self, request: &PermissionRequest) {
        if let (Some(id), Some(store)) = (request.decision_id, &self.permission_store) {
            // Off the runtime thread: nothing waits on this write.
            store.detach(move |store| {
                if let Err(error) = store.update_outcome(
                    id,
                    crate::permission::decisions::Outcome::Cancelled,
                    None,
                    None,
                ) {
                    tracing::warn!(%error, "permission decision outcome not recorded");
                }
            });
        }
    }

    fn settle_interrupted(&mut self, call: &ToolCall, remaining: &[ToolCall]) {
        for call in std::iter::once(call).chain(remaining) {
            self.transcript.push(ChatMessage::Tool {
                tool_call_id: call.id.clone(),
                content: String::new(),
                answered_by: None,
            });
        }
    }
}

/// Composes the role system message from the agent's prompt and the
/// editor's instructions (the editor has the last word); `None` when both
/// are empty.
pub fn compose_role_message(agent: Option<&str>, editor: Option<&str>) -> Option<String> {
    let agent = agent.map(str::trim).filter(|prompt| !prompt.is_empty());
    let editor = editor.filter(|prompt| !prompt.trim().is_empty());
    match (agent, editor) {
        (Some(agent), Some(editor)) => Some(format!("{agent}\n\n{editor}")),
        (Some(only), None) | (None, Some(only)) => Some(only.to_string()),
        (None, None) => None,
    }
}
