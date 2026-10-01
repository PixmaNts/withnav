//! Bounded per-actor inbox with reject-on-full backpressure (todo 4).
//!
//! One logical inbox (SPEC §15.2): one bounded `mpsc` transport per actor plus
//! an in-actor buffer of drained-but-not-yet-injected commands. Commands are
//! never replayable, so a full inbox REJECTS the next command (`BusError::
//! InboxFull`) instead of dropping or blocking. The drive loop is todo 5; this
//! module exposes the methods it calls.

use tokio::sync::mpsc;
use ulid::Ulid;

use crate::agent::actor::AgentActor;
use crate::bus::command::{Command, DowngradeNotice};
use crate::error::BusError;

pub const INBOX_CAPACITY: usize = 256;

/// What applying a drained batch did (besides pushing messages).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BatchOutcome {
    pub downgrade_notice: Option<DowngradeNotice>,
}

/// The sole holder of the inbox transport's receiver side. The todo-5 runtime
/// task owns this plus the `AgentActor`; the server/CLI only ever touch the
/// `Sender` returned by [`ActorInbox::channel`].
///
/// Holding the receiver inside keeps the bounded channel open (a dropped
/// receiver would make every later `try_send` a `Closed` error) and keeps
/// drain/apply explicit so the todo-5 drive loop controls the timing.
pub struct ActorInbox {
    commands: Vec<Command>,
    _rx: mpsc::Receiver<Command>,
}

/// Map a rejected enqueue onto the typed bus error the server turns into
/// `503 inbox_full` (ticket 7). Commands are rejected, never dropped or
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
    /// Create the bounded per-actor inbox transport: one `mpsc` channel plus
    /// the in-actor buffer, sharing a single FIFO order (SPEC §15.2).
    pub fn channel() -> (mpsc::Sender<Command>, Self) {
        let (tx, rx) = mpsc::channel(INBOX_CAPACITY);
        (
            tx,
            Self {
                commands: Vec::new(),
                _rx: rx,
            },
        )
    }

    /// Whether the in-actor buffer holds at least one command.
    pub fn has_buffered(&self) -> bool {
        !self.commands.is_empty()
    }

    /// The command_id of the FIRST buffered command — the task_id source for
    /// the todo-5 runtime (`active_task_id.or(first command_id)`).
    pub fn peek_first_buffered_command_id(&self) -> Option<Ulid> {
        self.commands.first().map(|cmd| cmd.command_id)
    }

    /// Drain everything currently in the transport into the in-actor buffer
    /// (FIFO). Returns how many commands were drained. The todo-5 drive loop
    /// calls this ONLY at a turn boundary — while the actor runs this stays
    /// uncalled, so mid-turn commands buffer for the next turn (next-turn
    /// semantics, no cancellation).
    pub fn drain_transport(&mut self) -> usize {
        while let Ok(cmd) = self._rx.try_recv() {
            self.commands.push(cmd);
        }
        self.commands.len()
    }

    /// The todo-5 drive loop's WAIT primitive: await the first command
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
        while let Ok(cmd) = self._rx.try_recv() {
            self.commands.push(cmd);
        }
        Some(self.commands.len())
    }

    /// Map the drained FIFO batch onto `push_user` semantics: the FIRST
    /// command after a Yield completes the pending ask (the existing
    /// `AgentActor` branch — untouched here), the rest are user messages.
    /// Stop is not a command: it goes through session cancellation.
    pub fn apply_batch_to_actor(&mut self, actor: &mut AgentActor) -> BatchOutcome {
        let mut outcome = BatchOutcome::default();
        for cmd in self.commands.drain(..) {
            if outcome.downgrade_notice.is_none() {
                outcome.downgrade_notice = cmd.downgrade_notice();
            }
            actor.push_user(cmd.text_content());
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::actor::AgentState;
    use crate::bus::command::{ContentBlock, DeliveryMode};
    use crate::provider::config::{ProviderConfig, ReasoningHistory};
    use crate::provider::openai::ChatMessage;
    use crate::tools::Registry;
    use std::collections::BTreeMap;
    use std::time::SystemTime;
    use ulid::Ulid;

    fn text_command(mode: DeliveryMode, text: &str) -> Command {
        Command {
            command_id: Ulid::new(),
            session_id: Ulid::new(),
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
                workspace: std::env::temp_dir(),
                writable_roots: vec![std::env::temp_dir()],
            },
            ProviderConfig {
                base_url: "http://127.0.0.1:1".into(),
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 100,
                request_timeout_ms: 100,
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
                ChatMessage::User { content } => Some(content.clone()),
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
        // The todo-5 drive loop drains ONLY at the turn boundary; while
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
        actor.pending_ask_id = Some("call_ask".into());

        inbox.drain_transport();
        inbox.apply_batch_to_actor(&mut actor);

        // First command completed the ask via the EXISTING push_user branch:
        assert!(matches!(
            actor.transcript.first(),
            Some(ChatMessage::Tool { tool_call_id, content })
                if tool_call_id == "call_ask" && content == "insert the card"
        ));
        // Rest are user messages, FIFO:
        assert!(matches!(
            actor.transcript.get(1),
            Some(ChatMessage::User { content }) if content == "second"
        ));
        assert!(matches!(
            actor.transcript.get(2),
            Some(ChatMessage::User { content }) if content == "third"
        ));
        assert_eq!(actor.transcript.len(), 3);
        assert_eq!(actor.pending_ask_id, None);
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
        assert_eq!(actor.pending_ask_id, None);

        inbox.drain_transport();
        let outcome = inbox.apply_batch_to_actor(&mut actor);
        let notice = outcome
            .downgrade_notice
            .expect("steer must produce a visible downgrade notice");
        assert_eq!(notice.mode, DeliveryMode::Steer);

        assert!(matches!(
            actor.transcript.last(),
            Some(ChatMessage::User { content }) if content == "lean left"
        ));
        assert_eq!(actor.pending_ask_id, None);
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
}
