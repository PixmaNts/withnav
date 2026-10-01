//! The data-driven decision pipeline (spec §Instinct): a TOML file plus its
//! referenced prompts, validated whole before it replaces the active
//! snapshot, evaluated as a pure function per trigger.
//!
//! - [`PipelineFile`]/[`TriggerDef`]/[`BranchDef`]: the TOML shape.
//! - [`validator`]: load-time checks (identifiers declared, boolean result,
//!   template variables known, first-matching-trigger semantics).
//! - [`evaluator`]: per-trigger evaluation (pre-filters, instinct call,
//!   branch choice).
//! - [`templates`]: `{{var}}` substitution over the fixed variable list.
//! - [`snapshot`]: TOML + referenced prompts, hashed; resolution project
//!   `.navigator/` > user dir > built-in.

use std::collections::BTreeMap;

use serde::Deserialize;

pub mod evaluator;
pub mod snapshot;
pub mod templates;
pub mod validator;

pub use evaluator::Pipeline;
pub use snapshot::{PipelineSnapshot, SnapshotSource};

/// Everything that can go wrong loading or validating a pipeline snapshot.
/// Load errors keep the previous snapshot running, so they carry the origin
/// (project file, user file, built-in) for the status bar.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PipelineError {
    #[error("pipeline io error ({origin}): {message}")]
    Io {
        origin: &'static str,
        message: String,
    },
    #[error("pipeline parse error ({origin}): {inner}")]
    Parse {
        origin: &'static str,
        #[source]
        inner: toml::de::Error,
    },
    #[error("pipeline invalid ({origin}): {message}")]
    Invalid {
        origin: &'static str,
        message: String,
    },
}

impl PipelineError {
    pub fn io(origin: &'static str, message: impl Into<String>) -> Self {
        Self::Io {
            origin,
            message: message.into(),
        }
    }

    pub fn invalid(origin: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            origin,
            message: message.into(),
        }
    }

    /// Where the failed snapshot came from ("project", "user", "builtin").
    pub fn origin(&self) -> &'static str {
        match self {
            Self::Io { origin, .. } | Self::Parse { origin, .. } | Self::Invalid { origin, .. } => {
                origin
            }
        }
    }
}

/// Which event fired. The pipeline's `on` lists these names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TriggerKind {
    Save,
    Idle,
    Diagnostics,
    Conversation,
}

impl TriggerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TriggerKind::Save => "save",
            TriggerKind::Idle => "idle",
            TriggerKind::Diagnostics => "diagnostics",
            TriggerKind::Conversation => "conversation",
        }
    }
}

/// The action a branch requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Silent,
    Remark,
    WakeCognition,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Silent => "silent",
            Action::Remark => "remark",
            Action::WakeCognition => "wake_cognition",
        }
    }
}

/// Pre-filter knobs of one trigger, evaluated before the instinct is called
/// (the prototype's fixed thresholds, now cheap and configurable).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Prefilters {
    /// Fewer changed lines than this skips the instinct.
    pub min_changed_lines: usize,
    /// A remark more recent than this (ms) skips the instinct.
    pub cooldown_ms: u64,
}

/// One ordered branch: `when` is an evalexpr condition over the flattened
/// answer variables plus the always-available set; first match wins.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchDef {
    pub when: String,
    pub action: ActionDef,
    /// Prompt for the metacognition (relative path), required for `remark`.
    #[serde(default)]
    pub prompt: Option<String>,
}

/// TOML spelling of [`Action`] (`silent | remark | wake_cognition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionDef {
    Silent,
    Remark,
    #[serde(alias = "wake_cognition")]
    WakeCognition,
}

impl From<ActionDef> for Action {
    fn from(a: ActionDef) -> Self {
        match a {
            ActionDef::Silent => Action::Silent,
            ActionDef::Remark => Action::Remark,
            ActionDef::WakeCognition => Action::WakeCognition,
        }
    }
}

/// One `[[trigger]]`: the events it handles, its pre-filters, state template
/// and typed questions.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerDef {
    /// Events this trigger handles: `save | idle | diagnostics | conversation`.
    pub on: Vec<String>,
    #[serde(default)]
    pub prefilter: Prefilters,
    /// Template rendering the state sent to the instinct.
    pub state: String,
    pub questions: BTreeMap<String, crate::navigator::ports::Question>,
    #[serde(default)]
    pub branch: Vec<BranchDef>,
}

/// The parsed `pipeline.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineFile {
    /// `instinct` table: the models.json alias of the instinct model.
    pub instinct: InstinctDef,
    #[serde(default, rename = "trigger")]
    pub triggers: Vec<TriggerDef>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstinctDef {
    pub model: String,
}

/// Always-available expression variables, independent of any question.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TriggerInputs {
    /// Changed-line count since the baseline.
    pub changed_lines: i64,
    /// Milliseconds since the last remark; a large value when there was none.
    pub since_last_remark_ms: i64,
    /// Whether the current diagnostics contain errors.
    pub has_errors: bool,
    /// Whether an intent record (or session intent) exists.
    pub has_intent: bool,
    /// Whether the conversation feed holds a digest (an active
    /// conversation with at least one exchange).
    pub has_conversation: bool,
}

/// Large "never" for `since_last_remark_ms` when no remark was delivered.
pub const NO_REMARK_MS: i64 = i64::MAX / 2;

/// How the pre-filters decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefilterOutcome {
    Pass,
    Skip(&'static str),
}

impl PrefilterOutcome {
    /// "pass" | "skip" for the log.
    pub fn as_str(&self) -> &'static str {
        match self {
            PrefilterOutcome::Pass => "pass",
            PrefilterOutcome::Skip(_) => "skip",
        }
    }

    pub fn skip_reason(&self) -> Option<&'static str> {
        match self {
            PrefilterOutcome::Skip(r) => Some(r),
            PrefilterOutcome::Pass => None,
        }
    }
}

/// The result of one pipeline evaluation. Everything the trigger log record
/// needs besides the delivered outcome.
#[derive(Debug, Clone)]
pub struct Evaluation {
    /// The action the chosen branch requested (silent for `no_branch` and
    /// pre-filter skips).
    pub requested_action: Action,
    /// Index of the matching branch, `None` when none matched.
    pub branch_index: Option<usize>,
    /// The instinct's answers, when the instinct was called.
    pub answers: Option<crate::navigator::ports::Answers>,
    /// Pre-filter decision.
    pub prefilter: PrefilterOutcome,
    /// The rendered state sent (or that would be sent) to the instinct.
    pub rendered_state: String,
    /// The structured template variable map (the inputs, not only text).
    pub template_inputs: BTreeMap<String, serde_json::Value>,
    /// Snapshot hash of the pipeline this evaluation ran under.
    pub hash: String,
}
