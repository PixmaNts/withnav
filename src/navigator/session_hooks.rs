//! The navigator's actor-profile policy for the session service: the
//! `navigator` profile defaults to the `cognition` alias, carries the
//! pair-programmer role prompt and the `intent` tool, and drops a legacy
//! copy of that prompt from a restored transcript.

use crate::agent::actor::AgentActor;
use crate::navigator::intent::IntentTool;
use crate::provider::openai::ChatMessage;
use crate::session::SessionHooks;

/// Session profile selecting the navigator's cognition: the `cognition`
/// model alias by default and the pair-programmer system prompt.
pub const NAVIGATOR_PROFILE: &str = "navigator";

/// The cognition's role in a navigator session, editable as Markdown.
pub const COGNITION_PROMPT: &str = include_str!("cognition.md");

/// [`SessionHooks`] for the navigator profile; other profiles are left plain.
#[derive(Debug, Clone, Copy, Default)]
pub struct NavigatorHooks;

fn is_navigator(profile: Option<&str>) -> bool {
    profile == Some(NAVIGATOR_PROFILE)
}

impl SessionHooks for NavigatorHooks {
    fn default_model(&self, profile: Option<&str>) -> Option<&str> {
        is_navigator(profile).then_some("cognition")
    }

    fn configure_actor(&self, profile: Option<&str>, actor: &mut AgentActor) {
        if !is_navigator(profile) {
            return;
        }
        // The navigator role rides as a second system message after the
        // agent loop's own contract; it is not part of the transcript.
        actor.system_prompt = Some(COGNITION_PROMPT.to_string());
        // The intent record is the navigator's working memory: only a
        // navigator-profile cognition may update it.
        actor.registry.register(Box::new(IntentTool));
        // A transcript saved before the role prompt left the transcript
        // starts with that prompt as a system message; drop it so the
        // actor does not send it twice.
        if matches!(
            actor.transcript.first(),
            Some(ChatMessage::System { content }) if content == COGNITION_PROMPT
        ) {
            actor.transcript.remove(0);
        }
    }
}
