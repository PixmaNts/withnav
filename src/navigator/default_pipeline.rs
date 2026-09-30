//! The built-in default pipeline: the three questions (`worth_attention`,
//! `route`, `drift`), pre-filters min 3 lines / cooldown 45 s, drift ->
//! wake_cognition (> 0.8, only if `has_intent`), remark when
//! `worth_attention > 0.85 && route == "metacognition"`, else silent.
//! The stricter values come from a first day of use (2026-09-29): at 0.7
//! and 20 s, `worth_attention` sat at 0.93-0.95 during a refactoring and the
//! metacognition remarked on every save. Replaying that log, 0.85 with a
//! 45 s cooldown halves the remarks (25 -> 13). Rewording the question to
//! discount mid-edit code silenced every remark, so the wording is kept.
//! Shipped in the binary via `include_str!`.

/// `pipeline.toml` as it would sit in `.navigator/`.
pub const PIPELINE_TOML: &str = include_str!("default_pipeline/pipeline.toml");

/// `prompts/state.md`: renders the state sent to the instinct.
pub const STATE_PROMPT: &str = include_str!("default_pipeline/prompts/state.md");

/// `prompts/remark.md`: the metacognition's remark prompt.
pub const REMARK_PROMPT: &str = include_str!("default_pipeline/prompts/remark.md");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_files_are_non_empty() {
        assert!(PIPELINE_TOML.contains("[instinct]"));
        assert!(STATE_PROMPT.contains("{{"));
        assert!(!REMARK_PROMPT.trim().is_empty());
    }
}
