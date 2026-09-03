//! Frozen `Command` shape and `DeliveryMode::{NextTurn, Steer, Interrupt}` (todo 4).
//!
//! Wire vocabulary per PLAN §7.3 / SPEC §6.4. Phase 2 implements NextTurn timing
//! only: `Steer`/`Interrupt` are accepted + recorded + visibly downgraded
//! (SPEC §15.3-15.4 semantics land in Phase 3).

use std::fmt;
use std::time::SystemTime;

use serde_json::Value;
use ulid::Ulid;

use crate::error::ServerError;

/// Actor identifier following the string grammar (`agent:main`, `user:cli`,
/// `user:http`). A string, NOT a ULID (PLAN §7.2).
pub type ActorId = String;

/// ULID-backed session identifier (PLAN §7.2).
pub type SessionId = Ulid;

/// How the sender wants the command timed. All three variants are frozen wire
/// vocabulary now; Phase 2 only implements NextTurn timing and downgrades the
/// other two (never rejects on mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    NextTurn,
    Steer,
    Interrupt,
}

/// Transcript content blocks. PROVISIONAL shape (PLAN §7.5): Phase 2 accepts
/// the full enum but only HANDLES `Text`; any other block is rejected at the
/// accept boundary with `BusError::NonTextContent`.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

/// A targeted, point-to-point instruction (SPEC §6.4). Distinct from an event:
/// never broadcast, never sequenced, never replayable — a rejected command is
/// the sender's to retry.
#[derive(Debug, Clone)]
pub struct Command {
    pub command_id: Ulid,
    pub session_id: SessionId,
    pub source_actor_id: ActorId,
    pub target_actor_id: ActorId,
    pub mode: DeliveryMode,
    pub content: Vec<ContentBlock>,
    pub created_at: SystemTime,
}

/// Visible downgrade marker for a `Steer`/`Interrupt` command in this build.
/// Surfaced in the message-received ack (todo 7) so clients see the mode was
/// recorded but runs at next-turn timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DowngradeNotice {
    pub mode: DeliveryMode,
}

impl fmt::Display for DowngradeNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.mode {
            DeliveryMode::NextTurn => "next-turn",
            DeliveryMode::Steer => "steer",
            DeliveryMode::Interrupt => "interrupt",
        };
        write!(
            f,
            "{mode} recorded; this build runs it at the next turn boundary"
        )
    }
}

/// What the runtime does with a routed command (ticket 02): every message
/// command maps onto `push_user`; a `stop` is the DISTINCT cancel signal
/// behind `POST /v1/sessions/:id/stop` (§20.2) — it cancels the actor token
/// (session teardown, todos 5/7) and must never become a transcript message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutedAction {
    PushUser,
    Stop,
}

impl Command {
    /// Inbox commands are message commands (POST /messages), so this is
    /// `PushUser` for every constructible Command; `RoutedAction::Stop`
    /// exists so the runtime/server route the stop ENDPOINT through the same
    /// enum instead of conflating cancel with a text push.
    pub fn routed_action(&self) -> RoutedAction {
        RoutedAction::PushUser
    }

    /// Phase-2 content rule: Text-only (PLAN §7.5). The server maps the typed
    /// error to 400 (ticket 7); the inbox rejects before buffering.
    pub fn validate_content(&self) -> Result<(), ServerError> {
        let non_text = self
            .content
            .iter()
            .any(|b| !matches!(b, ContentBlock::Text { .. }));
        if non_text {
            return Err(ServerError::NonTextContent);
        }
        Ok(())
    }

    /// Concat of the Text blocks (ticket 02: the loop wants a string, the wire
    /// wants typed blocks). Non-Text blocks never reach here in a valid batch:
    /// `validate_content` runs at the accept boundary.
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text } = block {
                out.push_str(text);
            }
        }
        out
    }

    /// Downgrade notice for this command's mode, `None` for NextTurn.
    pub fn downgrade_notice(&self) -> Option<DowngradeNotice> {
        match self.mode {
            DeliveryMode::NextTurn => None,
            DeliveryMode::Steer | DeliveryMode::Interrupt => {
                Some(DowngradeNotice { mode: self.mode })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn next_turn_mode_has_no_downgrade_notice() {
        let cmd = text_command(DeliveryMode::NextTurn, "hi");
        assert_eq!(cmd.downgrade_notice(), None);
    }

    #[test]
    fn steer_mode_is_recorded_and_visibly_downgraded() {
        let cmd = text_command(DeliveryMode::Steer, "lean left");
        let notice = cmd.downgrade_notice().expect("steer must be downgraded");
        assert_eq!(notice.mode, DeliveryMode::Steer);
        let rendered = notice.to_string();
        assert!(
            rendered.contains("steer"),
            "notice names the mode: {rendered}"
        );
        assert!(
            rendered.contains("next turn"),
            "notice names the timing: {rendered}"
        );
    }

    #[test]
    fn interrupt_mode_is_recorded_and_visibly_downgraded() {
        let cmd = text_command(DeliveryMode::Interrupt, "halt");
        let notice = cmd
            .downgrade_notice()
            .expect("interrupt must be downgraded");
        assert_eq!(notice.mode, DeliveryMode::Interrupt);
        assert!(notice.to_string().contains("interrupt"));
    }

    #[test]
    fn non_text_content_is_rejected_with_typed_error() {
        let mut cmd = text_command(DeliveryMode::NextTurn, "ok");
        cmd.content.push(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            args: Value::Null,
        });
        assert!(matches!(
            cmd.validate_content(),
            Err(ServerError::NonTextContent)
        ));
    }

    #[test]
    fn text_only_content_validates() {
        let cmd = text_command(DeliveryMode::NextTurn, "fine");
        assert!(
            cmd.validate_content().is_ok(),
            "text-only content must pass the boundary check"
        );
    }

    #[test]
    fn text_content_flattens_only_text_blocks() {
        let mut cmd = text_command(DeliveryMode::NextTurn, "a");
        cmd.content.push(ContentBlock::Thinking {
            text: "hidden".into(),
        });
        cmd.content.push(ContentBlock::Text { text: "b".into() });
        assert_eq!(cmd.text_content(), "ab");
    }
}
