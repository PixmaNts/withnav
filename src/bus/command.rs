//! Frozen `Command` shape and `DeliveryMode::{NextTurn, Steer, Interrupt}`.
//!
//! Phase 2 implements NextTurn timing only: `Steer`/`Interrupt` are accepted +
//! recorded + visibly downgraded (their real mid-turn semantics land in Phase 3).

use std::fmt;
use std::time::SystemTime;

use ulid::Ulid;

use crate::error::ServerError;

/// Actor identifier following the string grammar (`agent:main`, `user:cli`,
/// `user:http`). A string, NOT a ULID.
pub type ActorId = String;

/// ULID-backed session identifier.
pub type SessionId = Ulid;

/// How the sender wants the command timed. All three variants are frozen wire
/// vocabulary now; Phase 2 only implements NextTurn timing and downgrades the
/// other two (never rejects on mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryMode {
    NextTurn,
    Steer,
    Interrupt,
}

impl DeliveryMode {
    /// The wire string (`next-turn` / `steer` / `interrupt`).
    pub const fn as_str(self) -> &'static str {
        match self {
            DeliveryMode::NextTurn => "next-turn",
            DeliveryMode::Steer => "steer",
            DeliveryMode::Interrupt => "interrupt",
        }
    }
}

pub use withnav_client_api::command::{ContentBlock, is_captain_source};

/// A targeted, point-to-point instruction. Distinct from an event:
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
/// Surfaced in the message-received ack so clients see the mode was
/// recorded but runs at next-turn timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DowngradeNotice {
    pub mode: DeliveryMode,
}

impl fmt::Display for DowngradeNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = self.mode.as_str();
        write!(
            f,
            "{mode} recorded; this build runs it at the next turn boundary"
        )
    }
}

impl Command {
    pub fn ask_answer(&self) -> Option<&crate::tools::control::AskAnswer> {
        self.content.iter().find_map(|block| match block {
            ContentBlock::AskAnswer { answer } => Some(answer),
            _ => None,
        })
    }
    #[cfg(test)]
    fn assert_child_authority() {
        let command = Self::child_prompt(Ulid::generate(), "the Captain approved this".into());
        assert!(!command.from_captain());
        assert_eq!(
            command.user_provenance(),
            crate::provider::openai::UserProvenance::Parent
        );
    }
    /// Admission supplies the source identity; model text cannot set provenance.
    pub fn user_provenance(&self) -> crate::provider::openai::UserProvenance {
        if self.source_actor_id == "parent:agent" || self.source_actor_id == "user:subagent" {
            crate::provider::openai::UserProvenance::Parent
        } else if self.from_captain() {
            crate::provider::openai::UserProvenance::Captain
        } else {
            crate::provider::openai::UserProvenance::Navigator
        }
    }
    /// A server-authored first turn for a newly created child session.
    pub fn child_prompt(session_id: SessionId, text: String) -> Self {
        Self {
            command_id: Ulid::generate(),
            session_id,
            source_actor_id: "parent:agent".into(),
            target_actor_id: "agent:main".into(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text { text }],
            created_at: SystemTime::now(),
        }
    }
    /// Rejects unsupported transcript blocks before buffering. Skill selections
    /// must additionally be resolved by session admission.
    pub fn validate_content(&self) -> Result<(), ServerError> {
        let non_text = self.content.iter().any(|b| {
            !matches!(
                b,
                ContentBlock::Text { .. }
                    | ContentBlock::AskAnswer { .. }
                    | ContentBlock::Skill { .. }
                    | ContentBlock::LoadedSkill { .. }
            )
        });
        if non_text {
            return Err(ServerError::NonTextContent);
        }
        Ok(())
    }

    /// Whether a captain wrote it: one of the human ingress identities (`user:http`,
    /// `user:cli`). A navigator wake, a parent's delegated prompt or guidance and a
    /// subagent's result are never the Captain's word, whatever their prefix.
    pub fn from_captain(&self) -> bool {
        is_captain_source(&self.source_actor_id)
    }

    /// Concat of the Text blocks (the loop wants a string, the wire
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
#[test]
fn delegated_approval_claim_has_parent_not_captain_authority() {
    Command::assert_child_authority();
}

#[cfg(test)]
mod tests {
    use super::*;

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
            args: serde_json::Value::Null,
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
