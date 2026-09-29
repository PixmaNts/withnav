//! The built-in default pipeline (spec §Instinct): the three questions
//! (`worth_attention`, `route`, `drift`), pre-filters min 3 lines / cooldown
//! 20 s, drift -> wake_cognition (> 0.8, only if `has_intent`), remark when
//! `worth_attention > 0.7 && route == "metacognition"`, else silent.
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
