//! Bounded per-actor inbox with reject-on-full backpressure.
//!
//! One logical inbox: one bounded `mpsc` transport per actor plus
//! an in-actor buffer of drained-but-not-yet-injected commands. Commands are
//! never replayable, so a full inbox REJECTS the next command (`BusError::
//! InboxFull`) instead of dropping or blocking. The runtime drive loop calls the
//! methods this module exposes.

use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

use crate::agent::actor::AgentActor;
use crate::agent::permissions::{PermissionDecision, PermissionError};
use crate::bus::command::{Command, DowngradeNotice};
use crate::error::BusError;

pub const INBOX_CAPACITY: usize = 256;

/// How many decisions may wait for the runtime at once.
const ANSWER_CAPACITY: usize = 16;

/// A Captain's decision on one permission request, on its way to the runtime.
/// It is a typed message of its own, never a [`Command`]: no chat text can
/// take its place. The runtime answers on `reply` as soon as it has accepted
/// or refused the decision, before the resumed turn runs.
#[derive(Debug)]
pub struct PermissionAnswer {
    pub request_id: Ulid,
    pub decision: PermissionDecision,
    pub origin: crate::agent::permissions::PermissionOrigin,
    pub reply: oneshot::Sender<Result<(), PermissionError>>,
}

/// What woke the runtime.
#[derive(Debug)]
pub enum Inbound {
    Completion,
    Continue,
    /// Commands are buffered: apply them and run a turn.
    Batch(usize),
    /// A decision on a permission request.
    Answer(PermissionAnswer),
}

/// What applying a drained batch did (besides pushing messages).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BatchOutcome {
    pub refused: Vec<(Ulid, String)>,
    pub downgrade_notice: Option<DowngradeNotice>,
    /// How many commands reached the transcript: the rest were held.
    pub applied: usize,
    /// The first command that reached the transcript.
    pub first_applied: Option<Ulid>,
}

/// The sole holder of the inbox transport's receiver side. The runtime
/// task owns this plus the `AgentActor`; the server/CLI only ever touch the
/// `Sender` returned by [`ActorInbox::channel`].
///
/// Holding the receiver inside keeps the bounded channel open (a dropped
/// receiver would make every later `try_send` a `Closed` error) and keeps
/// drain/apply explicit so the runtime drive loop controls the timing.
pub struct ActorInbox {
    queued_captain: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    commands: Vec<Command>,
    /// The latest command that is not the Captain's, set aside while the
    /// actor waits on an `ask` or a permission request: such a command would
    /// answer the ask or cancel the request. Released by
    /// [`release_held`](Self::release_held) once nothing is pending.
    held: Option<Command>,
    _rx: mpsc::Receiver<Command>,
    answers_tx: mpsc::Sender<PermissionAnswer>,
    answers_rx: mpsc::Receiver<PermissionAnswer>,
}

/// Map a rejected enqueue onto the typed bus error the server turns into
/// `503 inbox_full`. Commands are rejected, never dropped or
/// blocked, because unlike ring events they cannot be replayed.
impl From<mpsc::error::TrySendError<Command>> for BusError {
    fn from(err: mpsc::error::TrySendError<Command>) -> Self {
        match err {
            mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_) => {
                BusError::InboxFull
            }
        }
    }
}

impl ActorInbox {
    pub(crate) fn track_captain(&mut self, count: std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        self.queued_captain = Some(count);
    }
    /// Create the bounded per-actor inbox transport: one `mpsc` channel plus
    /// the in-actor buffer, sharing a single FIFO order.
    pub fn channel() -> (mpsc::Sender<Command>, Self) {
        let (tx, rx) = mpsc::channel(INBOX_CAPACITY);
        let (answers_tx, answers_rx) = mpsc::channel(ANSWER_CAPACITY);
        (
            tx,
            Self {
                queued_captain: None,
                commands: Vec::new(),
                held: None,
                _rx: rx,
                answers_tx,
                answers_rx,
            },
        )
    }

    /// The sender of permission decisions for this actor. The inbox keeps
    /// one of its own, so the channel never reads as closed while the inbox
    /// lives.
    pub fn permission_sender(&self) -> mpsc::Sender<PermissionAnswer> {
        self.answers_tx.clone()
    }

    /// The runtime's wait primitive: a decision (they go first) or the next
    /// FIFO batch of commands. `None` only when every command sender is
    /// gone. Cancel safe.
    pub async fn next_inbound(&mut self) -> Option<Inbound> {
        if !self.commands.is_empty() {
            return Some(Inbound::Batch(self.drain_transport()));
        }
        tokio::select! {
            biased;
            Some(answer) = self.answers_rx.recv() => Some(Inbound::Answer(answer)),
            first = self._rx.recv() => {
                let first = first?;
                self.commands.push(first);
                Some(Inbound::Batch(self.drain_transport()))
            }
        }
    }

    /// Whether the in-actor buffer holds at least one command.
    pub fn has_buffered(&self) -> bool {
        !self.commands.is_empty()
    }
    /// Autonomous input cannot renew a captain-owned parked budget or pause.
    pub(crate) fn hold_autonomous(&mut self) {
        let mut captain = Vec::new();
        for command in std::mem::take(&mut self.commands) {
            if command.from_captain() {
                captain.push(command);
            } else {
                self.held = Some(command);
            }
        }
        self.commands = captain;
    }
    pub(crate) fn discard_held(&mut self) {
        self.held = None;
    }

    /// The command_id of the FIRST buffered command — the task_id source for
    /// the runtime (`active_task_id.or(first command_id)`).
    pub fn peek_first_buffered_command_id(&self) -> Option<Ulid> {
        self.commands.first().map(|cmd| cmd.command_id)
    }

    /// Drain everything currently in the transport into the in-actor buffer
    /// (FIFO). Returns how many commands were drained. The runtime drive loop
    /// calls this ONLY at a turn boundary — while the actor runs this stays
    /// uncalled, so mid-turn commands buffer for the next turn (next-turn
    /// semantics, no cancellation).
    pub fn drain_transport(&mut self) -> usize {
        while let Ok(cmd) = self._rx.try_recv() {
            self.commands.push(cmd);
        }
        self.commands.len()
    }

    /// The runtime drive loop's WAIT primitive: await the first command
    /// (yielding `None` only when all senders are gone), then scoop up
    /// everything already queued behind it into the in-actor buffer as one
    /// FIFO batch. The batch itself is applied by the existing
    /// `apply_batch_to_actor` — this only refills the buffer, so the runtime
    /// must never call it while a turn is mid-flight.
    pub async fn next_batch(&mut self) -> Option<usize> {
        if self.commands.is_empty() {
            let first = self._rx.recv().await?;
            self.commands.push(first);
        }
        Some(self.drain_transport())
    }

    /// Whether a held command waits for the actor to have nothing pending.
    pub fn has_held(&self) -> bool {
        self.held.is_some()
    }

    /// Moves the held command, if any, back into the buffer so the next
    /// [`next_inbound`](Self::next_inbound) delivers it. The runtime calls it
    /// only while the actor has nothing pending.
    pub fn release_held(&mut self) {
        if let Some(command) = self.held.take() {
            self.commands.push(command);
        }
    }

    /// Map the drained FIFO batch onto `push_user` semantics: the FIRST
    /// command after a Yield completes the pending ask (the existing
    /// `AgentActor` branch — untouched here), the rest are user messages.
    /// While the actor has a pending interaction, a command that is not the
    /// Captain's is held instead (the latest one is kept): it could neither
    /// answer an ask nor cancel a permission request. Captain text keeps its
    /// meaning. Stop is not a command: it goes through session cancellation.
    pub fn apply_batch_to_actor(&mut self, actor: &mut AgentActor) -> BatchOutcome {
        let mut outcome = BatchOutcome::default();

        for cmd in std::mem::take(&mut self.commands) {
            if cmd.from_captain() {
                if let Some(count) = &self.queued_captain {
                    // `fetch_update` is deprecated in Rust 1.99 in favour of `try_update`, which
                    // does not exist on the minimum supported Rust (1.89, Cargo.toml); keep the old
                    // name until the minimum moves past the rename.
                    #[allow(deprecated)]
                    let _ = count.fetch_update(
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                        |n| Some(n.saturating_sub(1)),
                    );
                }
            }
            if actor
                .role
                .as_ref()
                .is_some_and(|role| role.tools.get("skill") == Some(&false))
            {
                if let Some(name) = cmd.content.iter().find_map(|block| match block {
                    crate::bus::command::ContentBlock::LoadedSkill { name, .. } => Some(name),
                    _ => None,
                }) {
                    outcome.refused.push((cmd.command_id, format!("skill {name:?} refused: this turn's agent cannot use the skill tool; the whole message was not delivered")));
                    continue;
                }
            }
            if !cmd.from_captain() {
                if actor.has_pending() {
                    self.held = Some(cmd);
                    continue;
                }
                // A newer navigator command supersedes a held older one, even
                // when captain text in the same batch just cleared what was
                // pending: releasing the older one later would put stale
                // guidance after the newer.
                self.held = None;
            }
            if outcome.downgrade_notice.is_none() {
                outcome.downgrade_notice = cmd.downgrade_notice();
            }
            if outcome.first_applied.is_none() {
                actor.begin_skill_turn();
                outcome.first_applied = Some(cmd.command_id);
            }
            outcome.applied += 1;
            let mut text = cmd.text_content();
            if cmd.from_captain() {
                actor.captain_task = Some(text.clone());
            }
            for block in &cmd.content {
                if let crate::bus::command::ContentBlock::LoadedSkill { name, content } = block {
                    actor
                        .selected_skill_snapshots
                        .push((actor.skill_turn_generation, content.clone()));
                    let location = actor
                        .skills
                        .validation
                        .snapshots
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(content)
                        .map(|s| s.location.clone())
                        .unwrap_or_default();
                    let encoded =
                        serde_json::json!({"name":name,"location":location,"snapshot_id":content})
                            .to_string()
                            .replace('<', "\\u003c")
                            .replace('>', "\\u003e")
                            .replace('&', "\\u0026");
                    text.push_str("\n\n<captain_selected_skill encoding=\"json\">\n");
                    text.push_str(&encoded);
                    text.push_str("\n</captain_selected_skill>");
                }
            }
            let selected = cmd.content.iter().any(|block| {
                matches!(block, crate::bus::command::ContentBlock::LoadedSkill { .. })
            });
            if selected
                && matches!(
                    actor.pending,
                    Some(crate::agent::actor::PendingInteraction::Ask { .. })
                )
            {
                // Answer the existing tool interaction with captain text only; the selected
                // instructions remain a captain message protected as the latest batch.
                actor.push_user_in_current_turn(cmd.text_content(), cmd.user_provenance());
                actor
                    .transcript
                    .push(crate::provider::openai::ChatMessage::User {
                        content: text,
                        provenance: cmd.user_provenance(),
                    });
            } else {
                actor.push_user_in_current_turn(text, cmd.user_provenance());
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::actor::{AgentState, PendingInteraction};
    use crate::bus::command::{ContentBlock, DeliveryMode};
    use crate::provider::config::{ProviderConfig, ReasoningHistory};
    use crate::provider::openai::ChatMessage;
    use crate::tools::Registry;
    use std::collections::BTreeMap;
    use std::time::SystemTime;
    use ulid::Ulid;

    #[test]
    fn denying_pinned_role_leaves_pending_ask_and_captain_task_untouched() {
        let mut actor = test_actor();
        let mut role = (**crate::agent::definitions::Agents::builtin()
            .get("build")
            .unwrap())
        .clone();
        role.tools.insert("skill".into(), false);
        actor.role = Some(std::sync::Arc::new(role));
        actor.captain_task = Some("original task".into());
        actor.pending = Some(PendingInteraction::Ask {
            tool_call_id: "question".into(),
        });
        let before = serde_json::to_string(&actor.transcript).unwrap();
        let (_, mut inbox) = ActorInbox::channel();
        let mut denied = text_command(DeliveryMode::NextTurn, "denied answer");
        denied.content.push(ContentBlock::LoadedSkill {
            name: "review".into(),
            content: "denied body".into(),
        });
        inbox.commands.push(denied);
        let result = inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(result.refused.len(), 1);
        assert_eq!(result.applied, 0);
        assert!(matches!(
            actor.pending,
            Some(PendingInteraction::Ask { .. })
        ));
        assert_eq!(actor.captain_task.as_deref(), Some("original task"));
        assert_eq!(serde_json::to_string(&actor.transcript).unwrap(), before);
    }

    #[test]
    fn allowed_selection_answering_ask_keeps_instructions_in_captain_message() {
        let mut actor = test_actor();
        actor.pending = Some(PendingInteraction::Ask {
            tool_call_id: "question".into(),
        });
        let (_, mut inbox) = ActorInbox::channel();
        let mut selected = text_command(DeliveryMode::NextTurn, "my answer");
        selected.content.push(ContentBlock::LoadedSkill {
            name: "review".into(),
            content: "review body".into(),
        });
        inbox.commands.push(selected);
        inbox.apply_batch_to_actor(&mut actor);
        assert!(
            matches!(&actor.transcript[0], ChatMessage::Tool { content, .. } if content == "my answer")
        );
        assert!(
            matches!(&actor.transcript[1], ChatMessage::User { content, .. } if content.contains("review body") && content.starts_with("my answer"))
        );
        assert_eq!(
            crate::agent::compaction::latest_user_batch(&actor.transcript),
            Some(1..2)
        );
    }

    #[test]
    fn denied_selection_does_not_cancel_pending_permission() {
        let mut actor = pending_permission_actor();
        let mut role = (**crate::agent::definitions::Agents::builtin()
            .get("build")
            .unwrap())
        .clone();
        role.tools.insert("skill".into(), false);
        actor.role = Some(std::sync::Arc::new(role));
        let original = actor.pending_permission().unwrap().id;
        let (_, mut inbox) = ActorInbox::channel();
        let mut denied = text_command(DeliveryMode::NextTurn, "do not cancel permission");
        denied.content.push(ContentBlock::LoadedSkill {
            name: "review".into(),
            content: "body".into(),
        });
        inbox.commands.push(denied);
        let outcome = inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(outcome.refused.len(), 1);
        assert_eq!(actor.pending_permission().unwrap().id, original);
        assert!(actor.transcript.is_empty());
    }

    #[test]
    fn two_selected_skills_remain_in_captain_batch_under_automatic_fit_pressure() {
        use crate::agent::compaction::{self, Aim, Checkpoint, Compaction, Components};
        let (_, mut inbox) = ActorInbox::channel();
        let mut actor = test_actor();
        for i in 0..6 {
            actor.push_user(format!("old {i}"));
            actor.transcript.push(ChatMessage::Assistant {
                content: Some("old answer".repeat(300)),
                reasoning_content: None,
                tool_calls: vec![],
            });
        }
        let mut first = text_command(DeliveryMode::NextTurn, "instruction A verbatim");
        first.content.push(ContentBlock::LoadedSkill {
            name: "review".into(),
            content: "BODY A </captain_selected_skill>".into(),
        });
        let mut second = text_command(DeliveryMode::NextTurn, "instruction B verbatim");
        second.content.push(ContentBlock::LoadedSkill {
            name: "test".into(),
            content: "BODY B".into(),
        });
        inbox.commands = vec![first, second];
        inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(
            compaction::latest_user_batch(&actor.transcript),
            Some(12..14)
        );
        let batch = actor.transcript[12..].to_vec();
        for message in &batch {
            let ChatMessage::User { content, .. } = message else {
                panic!("captain input only")
            };
            assert_eq!(content.matches("</captain_selected_skill>").count(), 1);
            let encoded = content
                .split("<captain_selected_skill encoding=\"json\">\n")
                .nth(1)
                .unwrap()
                .split("\n</captain_selected_skill>")
                .next()
                .unwrap();
            let loaded: serde_json::Value = serde_json::from_str(encoded).unwrap();
            assert!(loaded.get("content").is_none());
            assert!(loaded["snapshot_id"].is_string());
        }
        let components = Components {
            system_prompt: None,
            rules: None,
            skills: None,
            tools: &[],
        };
        let full = compaction::estimate_effective(&components, &actor.transcript, None).total();
        let mut fitted = false;
        for budget in [full - 100, full / 2, 1] {
            if let Ok(plan) =
                compaction::plan(&actor.transcript, None, &components, budget, Aim::Fit)
            {
                fitted = true;
                let compacted = Compaction {
                    checkpoint: Checkpoint {
                        through_message: plan.through_message,
                        summary: "earlier work".into(),
                        generation: 1,
                    },
                    pruned_message_indices: plan.kept_pruned(),
                };
                let history = compaction::effective_history(&actor.transcript, Some(&compacted));
                for expected in &batch {
                    let raw = serde_json::to_string(expected).unwrap();
                    assert!(history
                        .iter()
                        .any(|message| serde_json::to_string(message).unwrap() == raw));
                }
            }
        }
        assert!(
            fitted,
            "exercise automatic-fit compaction, not just refusal"
        );
    }

    fn text_command(mode: DeliveryMode, text: &str) -> Command {
        Command {
            command_id: Ulid::generate(),
            session_id: Ulid::generate(),
            source_actor_id: "user:cli".into(),
            target_actor_id: "agent:main".into(),
            mode,
            content: vec![ContentBlock::Text { text: text.into() }],
            created_at: SystemTime::now(),
        }
    }

    fn test_actor() -> AgentActor {
        let mut registry = Registry::new();
        registry.register(Box::new(crate::tools::control::AskTool));
        AgentActor::new(
            registry,
            crate::tools::ToolContext {
                binding: None,
                workspace: std::env::temp_dir(),
                writable_roots: vec![std::env::temp_dir()],
                readable_roots: Vec::new(),
            },
            ProviderConfig {
                base_url: "http://127.0.0.1:1".into(),
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 100,
                request_timeout_ms: 100,
                catalog_format: None,
            },
            "test-key".into(),
            "test-model".into(),
            ReasoningHistory::Preserve,
            Some(64),
            tokio_util::sync::CancellationToken::new(),
        )
    }

    fn send(tx: &mpsc::Sender<Command>, cmd: Command) {
        tx.try_send(cmd).expect("inbox capacity");
    }

    fn user_texts(actor: &AgentActor) -> Vec<String> {
        actor
            .transcript
            .iter()
            .filter_map(|m| match m {
                ChatMessage::User { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    // Given a Running actor with commands arriving mid-turn
    // When one turn-boundary drain runs
    // Then all mid-turn commands leave the transport as one FIFO batch
    #[tokio::test]
    async fn running_actor_buffers_mid_turn_commands_until_turn_boundary() {
        let (tx, mut inbox) = ActorInbox::channel();
        for i in 0..3 {
            send(&tx, text_command(DeliveryMode::NextTurn, &format!("m{i}")));
        }
        // The runtime drive loop drains ONLY at the turn boundary; while
        // run_agent holds the actor RUNNING these stay buffered untouched:
        assert!(!inbox.has_buffered());
        assert_eq!(inbox.drain_transport(), 3);

        let mut actor = test_actor();
        actor.state = AgentState::Running;
        let outcome = inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(outcome.downgrade_notice, None);
        assert_eq!(user_texts(&actor), vec!["m0", "m1", "m2"]);
        assert!(!inbox.has_buffered());
    }

    // Given a command with a non-Text block at the accept boundary
    // When it is validated before enqueue
    // Then it is rejected with a typed error and never reaches the inbox
    #[tokio::test]
    async fn non_text_content_is_rejected_before_buffering() {
        let (tx, mut inbox) = ActorInbox::channel();
        let mut cmd = text_command(DeliveryMode::NextTurn, "ok");
        cmd.content = vec![ContentBlock::ToolResult {
            tool_call_id: "t".into(),
            content: "x".into(),
        }];
        assert!(
            cmd.validate_content().is_err(),
            "non-text must be rejected at the boundary"
        );
        // The server never try_sends a rejected command, so the transport and
        // the buffer stay empty:
        assert_eq!(tx.capacity(), INBOX_CAPACITY);
        assert!(!inbox.has_buffered());
        assert_eq!(inbox.drain_transport(), 0);
    }

    // Given a bounded inbox already at capacity
    // When one more command tries to enqueue
    // Then the enqueue is rejected with the typed InboxFull (the 503 mapping)
    //      and the already-buffered commands are all still there
    #[tokio::test]
    async fn full_inbox_rejects_the_next_command_without_blocking() {
        let (tx, mut inbox) = ActorInbox::channel();
        let mut accepted = 0;
        let rejected: mpsc::error::TrySendError<Command> = loop {
            let cmd = text_command(DeliveryMode::NextTurn, "x");
            let id = cmd.command_id;
            match tx.try_send(cmd) {
                Ok(()) => accepted += 1,
                Err(mpsc::error::TrySendError::Full(back)) => {
                    assert_eq!(
                        back.command_id, id,
                        "the rejected command is handed back, not dropped"
                    );
                    break mpsc::error::TrySendError::Full(back);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    unreachable!("ActorInbox holds the receiver");
                }
            }
        };
        assert!(accepted > 0, "inbox must accept up to its capacity first");
        // The typed rejection the server maps to 503 inbox_full:
        let err: BusError = rejected.into();
        assert!(matches!(err, BusError::InboxFull));
        // Nothing was lost — every accepted command survives the drain:
        assert_eq!(inbox.drain_transport(), accepted);
    }

    // Given a Yielded actor with a pending ask and three buffered commands
    // When one FIFO batch drains onto push_user
    // Then the first command answers the pending ask (role Tool) and the
    //      remaining two append as user messages, in FIFO order
    #[tokio::test]
    async fn drain_after_yield_answers_pending_ask_first_then_user_messages() {
        let (tx, mut inbox) = ActorInbox::channel();
        send(&tx, text_command(DeliveryMode::NextTurn, "insert the card"));
        send(&tx, text_command(DeliveryMode::NextTurn, "second"));
        send(&tx, text_command(DeliveryMode::NextTurn, "third"));

        let mut actor = test_actor();
        actor.state = AgentState::Yielded;
        actor.pending = Some(PendingInteraction::Ask {
            tool_call_id: "call_ask".into(),
        });

        inbox.drain_transport();
        inbox.apply_batch_to_actor(&mut actor);

        // First command completed the ask via the EXISTING push_user branch:
        assert!(matches!(
            actor.transcript.first(),
            Some(ChatMessage::Tool { tool_call_id, content, .. })
                if tool_call_id == "call_ask" && content == "insert the card"
        ));
        // Rest are user messages, FIFO:
        assert!(matches!(
            actor.transcript.get(1),
            Some(ChatMessage::User { content, .. }) if content == "second"
        ));
        assert!(matches!(
            actor.transcript.get(2),
            Some(ChatMessage::User { content, .. }) if content == "third"
        ));
        assert_eq!(actor.transcript.len(), 3);
        assert_eq!(actor.pending, None);
    }

    // Given an Idle actor with a steer command buffered (downgraded, Phase 2)
    // When the batch applies
    // Then the steer command is applied like any user message — never rejected
    //      on mode — and the downgrade notice names its recorded mode
    #[tokio::test]
    async fn steer_command_is_accepted_recorded_and_downgraded_to_next_turn() {
        let (tx, mut inbox) = ActorInbox::channel();
        let steer = text_command(DeliveryMode::Steer, "lean left");
        send(&tx, steer);

        let mut actor = test_actor();
        actor.state = AgentState::Idle;
        assert_eq!(actor.pending, None);

        inbox.drain_transport();
        let outcome = inbox.apply_batch_to_actor(&mut actor);
        let notice = outcome
            .downgrade_notice
            .expect("steer must produce a visible downgrade notice");
        assert_eq!(notice.mode, DeliveryMode::Steer);

        assert!(matches!(
            actor.transcript.last(),
            Some(ChatMessage::User { content, .. }) if content == "lean left"
        ));
        assert_eq!(actor.pending, None);
    }

    // Given a batch mixing a steer and plain next-turn commands
    // When the batch applies
    // Then the notice surfaces even when the downgraded command is not first
    #[tokio::test]
    async fn downgrade_notice_surfaces_from_anywhere_in_the_batch() {
        let (tx, mut inbox) = ActorInbox::channel();
        send(&tx, text_command(DeliveryMode::NextTurn, "first"));
        send(&tx, text_command(DeliveryMode::Interrupt, "halt"));
        send(&tx, text_command(DeliveryMode::NextTurn, "third"));

        let mut actor = test_actor();
        inbox.drain_transport();
        let outcome = inbox.apply_batch_to_actor(&mut actor);
        let notice = outcome
            .downgrade_notice
            .expect("interrupt must produce a downgrade notice");
        assert_eq!(notice.mode, DeliveryMode::Interrupt);
        assert_eq!(
            user_texts(&actor),
            vec!["first", "halt", "third"],
            "mode never reorders or drops the FIFO batch"
        );
    }

    // Given a parked next_batch() call with one command already queued
    // When a second command arrives before the wait is served
    // Then the buffer refills with BOTH commands in arrival order and
    //      apply_batch_to_actor applies them FIFO; a later
    //      all-senders-dropped resolves to None
    #[tokio::test]
    async fn next_batch_returns_one_fifo_batch_then_none_when_senders_drop() {
        let (tx, mut inbox) = ActorInbox::channel();
        send(&tx, text_command(DeliveryMode::NextTurn, "one"));
        send(&tx, text_command(DeliveryMode::NextTurn, "two"));

        assert_eq!(inbox.next_batch().await, Some(2));
        assert!(inbox.has_buffered(), "the batch stays buffered for apply");

        let mut actor = test_actor();
        inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(user_texts(&actor), vec!["one", "two"]);
        assert!(!inbox.has_buffered());

        drop(tx);
        assert!(
            inbox.next_batch().await.is_none(),
            "all senders dropped ends the drive loop"
        );
    }

    fn command_from(source: &str, text: &str) -> Command {
        let mut cmd = text_command(DeliveryMode::NextTurn, text);
        cmd.source_actor_id = source.into();
        cmd
    }

    fn pending_permission_actor() -> AgentActor {
        use crate::agent::permissions::{PermissionRequest, Scope};
        let mut actor = test_actor();
        actor.state = AgentState::Yielded;
        actor.pending = Some(PendingInteraction::Permission {
            request: Box::new(PermissionRequest {
                requires_captain: false,
                permission_gate: None,
                id: Ulid::generate(),
                tool_call_id: "w1".into(),
                tool: "write".into(),
                arguments: "{}".into(),
                scope: Scope::Folder("/w".into()),
                agent: "build".into(),
                reason: "r".into(),
                always_label: None,
                reusable_approval: false,
                always: Vec::new(),
                decision_id: None,
                repeats: 1,
                skill_rule: false,
            }),
            call: crate::provider::ToolCall {
                id: "w1".into(),
                name: "write".into(),
                args: serde_json::json!({}),
                args_valid: true,
            },
            remaining_calls: vec![],
            destination: None,
            steps_left: 3,
        });
        actor
    }

    // Given an actor waiting on a permission request or an ask
    // When navigator commands arrive
    // Then they are held, the latest one alone is kept, nothing reaches the
    //      transcript and the request stays open; once nothing is pending the
    //      held command is released
    #[tokio::test]
    async fn a_navigator_command_is_held_while_an_interaction_is_pending() {
        for mut actor in [pending_permission_actor(), {
            let mut a = test_actor();
            a.state = AgentState::Yielded;
            a.pending = Some(PendingInteraction::Ask {
                tool_call_id: "call_ask".into(),
            });
            a
        }] {
            let (tx, mut inbox) = ActorInbox::channel();
            send(&tx, command_from("navigator", "first wake"));
            send(&tx, command_from("navigator", "latest wake"));
            inbox.drain_transport();
            let before = actor.transcript.len();
            let outcome = inbox.apply_batch_to_actor(&mut actor);
            assert_eq!(outcome.applied, 0);
            assert!(actor.has_pending(), "the interaction stays open");
            assert_eq!(actor.transcript.len(), before);
            assert!(inbox.has_held());
            assert!(!inbox.has_buffered());

            actor.pending = None;
            inbox.release_held();
            assert!(!inbox.has_held());
            inbox.drain_transport();
            let outcome = inbox.apply_batch_to_actor(&mut actor);
            assert_eq!(outcome.applied, 1);
            assert_eq!(user_texts(&actor), vec!["latest wake"]);
        }
    }

    // Given a captain request, then a navigator wake, then a captain answer to an ask
    // When the batches apply
    // Then the task is the Captain's latest words, never the wake
    #[tokio::test]
    async fn the_captain_task_follows_the_captain_and_never_a_wake() {
        let (tx, mut inbox) = ActorInbox::channel();
        let mut actor = test_actor();
        send(&tx, command_from("user:http", "build the parser"));
        send(&tx, command_from("navigator", "drift: look at the lexer"));
        inbox.drain_transport();
        inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(actor.captain_task.as_deref(), Some("build the parser"));
        assert!(actor.transcript[0].is_captain_message());
        assert!(!actor.transcript[1].is_captain_message());
        let restored: Vec<ChatMessage> =
            serde_json::from_value(serde_json::to_value(&actor.transcript).unwrap()).unwrap();
        assert_eq!(
            crate::agent::compaction::latest_user_batch(&restored),
            Some(0..2)
        );
        assert_eq!(
            user_texts(&actor),
            vec!["build the parser", "drift: look at the lexer"]
        );

        actor.pending = Some(PendingInteraction::Ask {
            tool_call_id: "call_ask".into(),
        });
        send(&tx, command_from("user:cli", "use the local scope"));
        inbox.drain_transport();
        inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(
            actor.captain_task.as_deref(),
            Some("use the local scope"),
            "an answer to an ask is the Captain speaking"
        );
        send(&tx, command_from("navigator", "another wake"));
        inbox.drain_transport();
        inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(actor.captain_task.as_deref(), Some("use the local scope"));
    }

    // Given a pending permission request
    // When captain text arrives, after a navigator command
    // Then the Captain text cancels the request as before, and the navigator
    //      command that came before it was held, not applied
    #[tokio::test]
    async fn captain_text_still_cancels_a_pending_permission_request() {
        let (tx, mut inbox) = ActorInbox::channel();
        send(&tx, command_from("navigator", "wake"));
        send(&tx, command_from("user:http", "never mind"));
        inbox.drain_transport();
        let mut actor = pending_permission_actor();
        let outcome = inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(outcome.applied, 1);
        assert!(!actor.has_pending(), "the Captain's text settled it");
        assert_eq!(user_texts(&actor), vec!["never mind"]);
        assert!(inbox.has_held());
    }

    // Given a pending permission request
    // When one batch brings an older navigator command, captain text (which
    //      cancels the request) and a newer navigator command
    // Then the newer navigator command is applied after the Captain text and
    //      the older one is dropped: it is never released after the newer
    #[tokio::test]
    async fn a_newer_navigator_command_supersedes_a_held_older_one() {
        let (tx, mut inbox) = ActorInbox::channel();
        send(&tx, command_from("navigator", "wake A"));
        send(&tx, command_from("user:http", "never mind"));
        send(&tx, command_from("navigator", "wake B"));
        inbox.drain_transport();
        let mut actor = pending_permission_actor();
        let outcome = inbox.apply_batch_to_actor(&mut actor);
        assert_eq!(outcome.applied, 2);
        assert_eq!(user_texts(&actor), vec!["never mind", "wake B"]);
        assert!(!inbox.has_held(), "the older wake is superseded");
        inbox.release_held();
        assert!(!inbox.has_buffered());
    }
}
