//! The built-in default pipeline: the six questions (`worth_attention`,
//! `route`, `drift`, `contradicts_conversation`, `unfinished`, `stuck`), pre-filters min 3
//! lines / remark cooldown 45 s, then the branches in order:
//!
//! 0. `remark` (prompt `help_remark.md`) when `called_for_help`: the lines
//!    the driver just added hold a comment addressing the navigator
//!    ("help", "aide", "navigator", a question). The first trigger's
//!    `bypass_on_help_call` lets such a change past `min_changed_lines` and
//!    the remark cooldown (a one-line `// J'ai besoin d'aide` was skipped 92
//!    times in a real evening), and this branch comes first so no score can
//!    turn the call away. It stays a remark, never a wake: the driver is at
//!    the editor, not in the chat.
//! 1. `wake_cognition` when `stuck > 0.8`: the instinct judges the driver
//!    stuck (the same errors persist or come back across recent edits,
//!    the same lines are undone and retried, a comment shows frustration).
//!    It wakes whether the code compiles or not: a driver who does not
//!    compile and struggles is who the chat is for. The wake message says
//!    "you look stuck" and carries no intent section.
//! 2. `wake_cognition` on drift from the intent (> 0.8, only if
//!    `has_intent`) or a contradiction with what the driver and the
//!    cognition just agreed (> 0.8, only if `has_conversation`: without a
//!    conversation digest there is nothing to contradict), and only if
//!    `unfinished < 0.5`: the instinct judges whether the edit is one the
//!    driver is still typing (a half-written statement, a copied body not
//!    yet adapted). Compile errors alone do not make it unfinished, and
//!    `has_errors` stays a variable no default branch uses. Wakes have
//!    their own cooldown, `wake_cooldown_ms` = 120 s, apart from the remark
//!    cooldown: a wake inside it does not match and the trigger falls to
//!    the next branch, and a wake never silences the next remark.
//! 3. `remark` when `unfinished < 0.5 && worth_attention > 0.85 &&
//!    route != "silent"`. The route `cognition` without a wake (scores under
//!    the wake thresholds) still gets its remark: the metacognition writes
//!    it, the cognition is only woken by branches 1 and 2.
//! 4. silent.
//!
//! All questions travel in one instinct call. `stuck` reads the diagnostics
//! and the live state (recent edits, newest first, and error/warning
//! diagnostics) the state template already gives.
//!
//! The instinct and the metacognition both see the conversation digest and
//! a live state of recent edits and diagnostics, so their judgment is
//! anchored on what the layers already agreed rather than the diff alone.
//! The stricter values come from a first day of use (2026-09-29): at 0.7
//! and 20 s, `worth_attention` sat at 0.93-0.95 during a refactoring and the
//! metacognition remarked on every save. Replaying that log, 0.85 with a
//! 45 s cooldown halves the remarks (25 -> 13). Rewording the question to
//! discount mid-edit code silenced every remark, so the wording is kept.
//!
//! A second trigger listens to `conversation` (a driver turn ended in the
//! chat): one question, `code_contradicts_conversation`, and a `remark`
//! when the code already contradicts what was just agreed (> 0.8, only
//! if `has_conversation`), else silent. It is a remark and not a wake
//! because the cognition already knows the conversation; the driver is
//! the one who needs to see the line. Its pre-filter is the 30 s cooldown
//! alone: the change is in the conversation, not in the file, so
//! `min_changed_lines` has nothing to count.
//! Shipped in the binary via `include_str!`.

/// `pipeline.toml` as it would sit in `.navigator/`.
pub const PIPELINE_TOML: &str = include_str!("default_pipeline/pipeline.toml");

/// `prompts/state.md`: renders the state sent to the instinct.
pub const STATE_PROMPT: &str = include_str!("default_pipeline/prompts/state.md");

/// `prompts/remark.md`: the metacognition's remark prompt.
pub const REMARK_PROMPT: &str = include_str!("default_pipeline/prompts/remark.md");

/// `prompts/help_remark.md`: the metacognition's prompt when the driver's
/// comment calls on it.
pub const HELP_REMARK_PROMPT: &str = include_str!("default_pipeline/prompts/help_remark.md");

/// `prompts/conversation_state.md`: the instinct's state for the
/// `conversation` event.
pub const CONVERSATION_STATE_PROMPT: &str =
    include_str!("default_pipeline/prompts/conversation_state.md");

/// `prompts/conversation_remark.md`: the metacognition's remark prompt for
/// the `conversation` event.
pub const CONVERSATION_REMARK_PROMPT: &str =
    include_str!("default_pipeline/prompts/conversation_remark.md");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_files_are_non_empty() {
        assert!(PIPELINE_TOML.contains("[instinct]"));
        assert!(STATE_PROMPT.contains("{{"));
        assert!(!REMARK_PROMPT.trim().is_empty());
    }

    #[test]
    fn the_state_gives_the_instinct_what_stuck_needs() {
        assert!(STATE_PROMPT.contains("{{diagnostics}}"));
        assert!(STATE_PROMPT.contains("{{live_state}}"));
    }
}
