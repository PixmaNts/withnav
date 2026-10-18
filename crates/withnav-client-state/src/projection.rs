//! The per-session fields a client derives from the event stream, and the small rules that keep
//! them ordered: a value set by an event is not overwritten by a summary read at an older seq.
use std::collections::VecDeque;

use withnav_client_api::{
    event::{McpStatus, PauseToken},
    protocol::{CommandResponse, ContextSnapshot, ModelEntry, ModelIdentity},
};

use crate::{
    children::ChildAgent,
    conversation::{Activity, BlockKind, Conversation, PermissionPrompt},
    strip_controls,
};

/// The agent a conversation runs as when nothing else is known.
pub const DEFAULT_AGENT: &str = "build";

/// What a client knows of one conversation from its event stream, plus the selections it made
/// itself that the stream has yet to confirm. Each field is either set by an event or correlates
/// a client request with the event that settles it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionState {
    pub bypass: bool,
    pub bypass_seq: u64,
    pub effective: Option<ModelIdentity>,
    pub model_source: String,
    pub variant_source: String,
    pub selected_model_id: Option<String>,
    pub selected_model_name: Option<String>,
    pub context_window: Option<u64>,
    pub conversation: Conversation,
    pub activity: Activity,
    pub last_applied_seq: Option<u64>,
    /// The primary agent the conversation runs as, or starts as; `None` is build.
    pub agent: Option<String>,
    /// The model variant the conversation runs with, or starts with; `None` is none.
    pub variant: Option<String>,
    /// The variants the conversation's server offers (`/variant` answered `offered`); `None` until
    /// asked. The `/variant` list shows these, not the catalog's.
    pub variant_offer: Option<Vec<String>>,
    /// The seq of the last event that set `agent` / `variant`: a session summary read at an
    /// older seq does not overwrite them.
    pub agent_seq: Option<u64>,
    pub variant_seq: Option<u64>,
    /// The last `/model` this client sent, from the send until the server settles it: the header
    /// shows `→ model_id (after this turn)` once it is accepted for after the running turn.
    pub model_switch: Option<ModelSwitch>,
    /// The `/model` operations the stream reported as settled (done, refused, cancelled, or
    /// carried by a `model.changed`), the last few: an HTTP answer arriving after its own result
    /// does not mark a settled switch as waiting.
    pub settled_model_ops: VecDeque<String>,
    /// The seq of the last `model.changed`: a session summary read at an older seq does not
    /// overwrite the model.
    pub model_seq: Option<u64>,
    /// The request an answer was sent for, until `permission.resolved` ends it (or the answer
    /// did not reach the server): its keys are ignored meanwhile, another request's are not.
    pub answered_permission: Option<String>,
    /// Agents started by this conversation, shown under it; empty until the server reports them.
    pub children: Vec<ChildAgent>,
    /// A watchdog doubt or renewable step budget, never an ask or permission.
    pub watchdog_paused: bool,
    pub pause_token: Option<PauseToken>,
}

impl SessionState {
    /// The oldest permission request still open, if any.
    pub fn open_permission(&self) -> Option<&PermissionPrompt> {
        self.conversation.open_permissions.iter().find_map(|id| {
            self.conversation.blocks.iter().find_map(|b| match &b.kind {
                BlockKind::Permission(p) if &b.id == id && p.is_open() => Some(p),
                _ => None,
            })
        })
    }

    /// The primary agent's name for display; `default_agent` is the server's default, if known.
    pub fn agent_name_or<'a>(&'a self, default_agent: Option<&'a str>) -> &'a str {
        if let Some(effective) = &self.effective {
            return &effective.agent;
        }
        self.agent
            .as_deref()
            .or(default_agent)
            .unwrap_or(DEFAULT_AGENT)
    }

    /// The model_id a switch accepted for after this turn goes to, while it waits.
    pub fn pending_model(&self) -> Option<&str> {
        self.model_switch
            .as_ref()
            .filter(|m| m.deferred)
            .map(|m| m.model_id.as_str())
    }
}

/// A `/model` this client sent and the server has not settled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelSwitch {
    pub agent: String,
    pub model_id: String,
    /// The token of the request that asked for it: only its HTTP answer binds the operation.
    pub token: u64,
    /// The operation the server gave the command; `None` until its HTTP answer.
    pub operation_id: Option<String>,
    /// The server accepted it for after the running turn.
    pub deferred: bool,
}

/// The settled `/model` operations kept, at most.
const SETTLED_MODEL_OPS: usize = 16;

pub fn keep_bypass(s: &mut SessionState, enabled: bool, seq: u64) {
    if seq >= s.bypass_seq {
        s.bypass = enabled;
        s.bypass_seq = seq;
    }
}

/// Records that the `/model` operation `id` is settled; ends the switch waiting on it.
pub fn settle_model_op(s: &mut SessionState, id: &str) {
    if !s.settled_model_ops.iter().any(|o| o == id) {
        s.settled_model_ops.push_back(id.to_string());
        while s.settled_model_ops.len() > SETTLED_MODEL_OPS {
            s.settled_model_ops.pop_front();
        }
    }
    if s.model_switch
        .as_ref()
        .is_some_and(|m| m.operation_id.as_deref() == Some(id))
    {
        s.model_switch = None;
    }
}

/// The HTTP answer to this client's `/model` request `token`: it names the operation the switch
/// waits on. An acceptance for after the turn waits for that operation to settle on the stream,
/// unless it already did; anything else is settled now. The answer to an older request, replaced
/// since, changes nothing.
pub fn model_answered(s: &mut SessionState, token: u64, response: &CommandResponse) {
    let id = response.operation_id.clone();
    let deferred = response.ok
        && response
            .data
            .as_ref()
            .is_some_and(|d| d["accepted"].as_bool() == Some(true) && d["applies"] == "after_turn");
    let settled = s.settled_model_ops.contains(&id);
    let Some(switch) = s
        .model_switch
        .as_mut()
        .filter(|m| m.token == token && m.operation_id.is_none())
    else {
        return;
    };
    if deferred && !settled {
        switch.operation_id = Some(id);
        switch.deferred = true;
    } else {
        s.model_switch = None;
    }
}

/// Forgets that an answer was sent for `request_id`, if it was the one.
pub fn clear_answered(s: &mut SessionState, request_id: &str) {
    if s.answered_permission.as_deref() == Some(request_id) {
        s.answered_permission = None;
    }
}

/// Keeps `snapshot` unless the one held is newer: a higher revision of the same service
/// incarnation. A snapshot of another incarnation (the server restarted) always replaces it.
/// The read at connection and the live event can arrive in either order; neither goes back.
/// Server text is drawn on the terminal: control characters are removed first.
pub fn keep_newer_mcp(mcp: &mut Option<McpStatus>, mut snapshot: McpStatus) {
    let older = mcp.as_ref().is_some_and(|held| {
        held.incarnation == snapshot.incarnation && held.revision > snapshot.revision
    });
    if older {
        return;
    }
    for server in &mut snapshot.servers {
        server.server_id = strip_controls(&server.server_id);
        server.state = strip_controls(&server.state);
        server.error = server.error.as_deref().map(strip_controls);
        server.stderr_tail = strip_controls(&server.stderr_tail);
        for skipped in &mut server.skipped_tools {
            *skipped = strip_controls(skipped);
        }
    }
    *mcp = Some(snapshot);
}

/// Keeps what a `/variant` answer says the conversation offers.
pub fn set_variant_offer(s: &mut SessionState, data: &serde_json::Value) {
    if let Some(offered) = data["offered"].as_array() {
        s.variant_offer = Some(
            offered
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(strip_controls)
                .collect(),
        );
    }
}

/// How many retired incarnations the conversation remembers.
const RETIRED_INCARNATIONS: usize = 8;

/// What a resync keeps of the gauge.
pub struct ContextKept {
    snapshot: Option<ContextSnapshot>,
    epoch: u64,
    retired: std::collections::VecDeque<ulid::Ulid>,
}

pub fn take_context(s: &mut SessionState) -> ContextKept {
    ContextKept {
        snapshot: s.conversation.context.take(),
        epoch: s.conversation.context_epoch,
        retired: std::mem::take(&mut s.conversation.retired_incarnations),
    }
}

pub fn restore_context(s: &mut SessionState, kept: ContextKept) {
    s.conversation.context = kept.snapshot;
    s.conversation.context_epoch = kept.epoch;
    s.conversation.retired_incarnations = kept.retired;
}

/// Shows `snapshot` unless it is stale. Incarnation ids carry no order, so staleness is
/// decided here: within the incarnation shown, a lower revision is stale; a snapshot of a
/// new incarnation replaces the one shown, which is then retired; a snapshot of one of the
/// last eight retired incarnations is never shown again, however late it arrives (an older
/// HTTP answer is also dropped by its fetch epoch).
pub fn keep_newer_context(s: &mut SessionState, snapshot: ContextSnapshot) {
    let conversation = &mut s.conversation;
    if conversation
        .retired_incarnations
        .contains(&snapshot.incarnation)
    {
        return;
    }
    if let Some(held) = &conversation.context {
        if held.incarnation == snapshot.incarnation {
            if snapshot.revision < held.revision {
                return;
            }
        } else {
            conversation
                .retired_incarnations
                .push_back(held.incarnation);
            while conversation.retired_incarnations.len() > RETIRED_INCARNATIONS {
                conversation.retired_incarnations.pop_front();
            }
        }
    }
    conversation.context = Some(snapshot);
}

/// Records the model selection the server reported (or a session summary carried) at `seq`,
/// unless the one held is newer. `catalog` supplies the context window when neither the event nor
/// an earlier commit gave one.
pub fn apply_selection(
    s: &mut SessionState,
    selection: ModelIdentity,
    seq: u64,
    window: Option<u64>,
    catalog: &[ModelEntry],
) {
    if s.model_seq.is_some_and(|held| seq < held) {
        return;
    }
    let agent = strip_controls(&selection.agent);
    let model = strip_controls(&selection.selection.model);
    let committed_window = (s.selected_model_id.as_deref() == Some(model.as_str()))
        .then_some(s.context_window)
        .flatten();
    s.agent = Some(agent);
    s.selected_model_id = Some(model.clone());
    s.selected_model_name = Some(model.clone());
    s.variant = selection.selection.variant.as_deref().map(strip_controls);
    s.model_source = strip_controls(&selection.selection.source);
    s.variant_source = strip_controls(&selection.selection.variant_source);
    s.context_window = window.or(committed_window).or_else(|| {
        catalog
            .iter()
            .find(|entry| entry.name == model)
            .and_then(|entry| entry.context_window)
    });
    s.model_seq = Some(seq);
    s.agent_seq = Some(seq);
    s.variant_seq = Some(seq);
    s.effective = Some(selection);
}
