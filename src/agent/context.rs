//! The context budget: what the next request will weigh, by category, and how
//! much of the model's window it may fill.
//!
//! Everything here is pure. The estimate is a size guess (UTF-8 bytes / 4,
//! rounded up, plus a fixed framing per message), not a tokenizer: it is
//! labelled an estimate wherever it is shown, and it is never mixed with the
//! provider's own usage numbers.

use crate::agent::prompt::SYSTEM_CONTRACT;
pub use crate::bus::event::{Breakdown, ContextSnapshot, LastUsage, SkillCounts, tokens_k};
use crate::provider::openai::{ChatMessage, ToolDef};

/// Tokens added to every message for its role and delimiters.
pub const MESSAGE_FRAMING_TOKENS: u64 = 4;
/// Output reserve cap when `models.json` sets no output limit.
pub const DEFAULT_OUTPUT_RESERVE: u64 = 8192;
/// Smallest safety margin, in tokens, for windows of 32k and up. A smaller
/// window gets an eighth of itself instead, so the floor never eats half of an
/// 8k model's window.
pub const MIN_MARGIN: u64 = 4096;

/// UTF-8 bytes / 4, rounded up.
pub fn text_tokens(text: &str) -> u64 {
    bytes_tokens(text.len())
}

fn bytes_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

/// One message: its text, reasoning and tool calls, plus the framing.
pub fn message_tokens(message: &ChatMessage) -> u64 {
    let bytes = match message {
        ChatMessage::System { content } | ChatMessage::User { content, .. } => content.len(),
        ChatMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            content.as_deref().map_or(0, str::len)
                + reasoning_content.as_deref().map_or(0, str::len)
                + tool_calls
                    .iter()
                    .map(|c| c.id.len() + c.function.name.len() + c.function.arguments.len())
                    .sum::<usize>()
        }
        ChatMessage::Tool {
            tool_call_id,
            content,
            ..
        } => tool_call_id.len() + content.len(),
    };
    bytes_tokens(bytes) + MESSAGE_FRAMING_TOKENS
}

/// The tool definitions as they are serialized into the request.
pub fn tools_tokens(tools: &[ToolDef]) -> u64 {
    let mut counter = ByteCounter(0);
    for tool in tools {
        serde_json::to_writer(&mut counter, tool)
            .expect("tool definitions contain only JSON values and strings");
    }
    bytes_tokens(counter.0)
}

struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The system category: the contract plus the role message, if any.
pub fn system_tokens(role_message: Option<&str>) -> u64 {
    let framed = |text: &str| text_tokens(text) + MESSAGE_FRAMING_TOKENS;
    framed(SYSTEM_CONTRACT) + role_message.map_or(0, framed)
}

/// Estimates the request `prompt::build_request` assembles from these parts.
pub fn estimate(
    system_prompt: Option<&str>,
    rules: Option<&str>,
    skills: Option<&str>,
    history: &[ChatMessage],
    tools: &[ToolDef],
) -> Breakdown {
    let framed = |text: &str| text_tokens(text) + MESSAGE_FRAMING_TOKENS;
    Breakdown {
        system: system_tokens(system_prompt)
            + if history.iter().any(|m| {
                matches!(
                    m,
                    ChatMessage::User {
                        provenance: crate::provider::openai::UserProvenance::Subagent,
                        ..
                    }
                )
            }) {
                text_tokens(crate::agent::prompt::SUBAGENT_RULE) + 1
            } else {
                0
            },
        rules: rules.map_or(0, framed),
        skills_catalog: skills.map_or(0, framed),
        tools: tools_tokens(tools),
        messages: history.iter().map(message_tokens).sum(),
    }
}

/// How much of the window a request may fill, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Budget {
    pub window: u64,
    /// Room kept for the answer.
    pub output_reserve: u64,
    /// Room kept for the estimate being wrong.
    pub margin: u64,
    /// The most the next request's estimate may reach.
    pub input_budget: u64,
}

/// A window too small for its own reserve and margin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetError {
    pub window: u64,
    pub output_reserve: u64,
    pub margin: u64,
    pub max_input: Option<u64>,
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "context window {} leaves no room for input: it must hold an output reserve of {}, a safety margin of {}",
            self.window, self.output_reserve, self.margin
        )?;
        if let Some(max_input) = self.max_input {
            write!(f, " and stay within the input limit of {max_input}")?;
        }
        write!(
            f,
            "; choose a model with a larger context window. Correct \"context_window\" in models.json only if the configured value is wrong, and lower \"max_output_tokens\" if it is what takes the room"
        )
    }
}

impl std::error::Error for BudgetError {}

/// R: the `models.json` output limit; else the smaller of 8192, a quarter of
/// the window and the output ceiling the provider reports.
pub fn output_reserve(window: u64, configured: Option<u64>, discovered: Option<u64>) -> u64 {
    if let Some(limit) = configured.filter(|n| *n > 0) {
        return limit;
    }
    let mut reserve = DEFAULT_OUTPUT_RESERVE.min(window / 4);
    if let Some(ceiling) = discovered.filter(|n| *n > 0) {
        reserve = reserve.min(ceiling);
    }
    reserve
}

/// M: 5% of the window, at least [`MIN_MARGIN`] (an eighth of the window when
/// that is smaller).
pub fn margin(window: u64) -> u64 {
    MIN_MARGIN.min(window / 8).max(window.div_ceil(20))
}

impl Budget {
    /// B = min(W − R, I or W) − M, where W is the window, R the output
    /// reserve, I the input limit and M the margin. A budget of zero or less
    /// is an error, never clamped.
    pub fn compute(
        window: u64,
        max_input: Option<u64>,
        configured_output: Option<u64>,
        discovered_output: Option<u64>,
    ) -> Result<Self, BudgetError> {
        let output_reserve = output_reserve(window, configured_output, discovered_output);
        let margin = margin(window);
        let max_input = max_input.filter(|n| *n > 0);
        let ceiling = window
            .saturating_sub(output_reserve)
            .min(max_input.unwrap_or(window));
        if ceiling <= margin {
            return Err(BudgetError {
                window,
                output_reserve,
                margin,
                max_input,
            });
        }
        Ok(Self {
            window,
            output_reserve,
            margin,
            input_budget: ceiling - margin,
        })
    }
}

impl ContextLimits {
    /// The limits of a session from its saved resolution. `configured_output`
    /// is the alias's `max_output_tokens` in models.json. Fails when the
    /// window cannot hold the reserve and the margin.
    pub fn from_resolution(
        alias: &str,
        variant: Option<&str>,
        resolution: &crate::provider::catalog::ModelResolution,
        configured_output: Option<u64>,
    ) -> Result<Self, BudgetError> {
        let bounds = &resolution.bounds;
        Ok(Self {
            alias: alias.to_string(),
            gateway: resolution.provider.clone(),
            upstream_id: resolution.model.clone(),
            variant: variant.map(str::to_string),
            window_source: resolution.provenance.context_window.clone(),
            budget: Budget::compute(
                bounds.context_window,
                bounds.max_input_tokens,
                configured_output,
                bounds.reported_output,
            )?,
            skills: SkillCounts::default(),
            rule_files: 0,
        })
    }

    /// Adds what the session's rules and skills snapshots hold.
    pub fn with_sources(
        mut self,
        rules: &crate::agent::instructions::Instructions,
        skills: &crate::agent::skills::Skills,
    ) -> Self {
        let (listed, omitted) = skills.catalog_counts();
        self.skills = SkillCounts {
            listed: listed as u64,
            omitted: omitted as u64,
        };
        self.rule_files = rules.contributing_files() as u64;
        self
    }
}

/// The snapshot of one request estimate under a session's model limits.
pub fn context_snapshot(
    limits: &ContextLimits,
    breakdown: Breakdown,
    last_usage: Option<LastUsage>,
    incarnation: ulid::Ulid,
    revision: u64,
) -> ContextSnapshot {
    let estimate_total = breakdown.total();
    ContextSnapshot {
        model: format!("{}/{}", limits.gateway, limits.upstream_id),
        gateway: limits.gateway.clone(),
        upstream_id: limits.upstream_id.clone(),
        variant: limits.variant.clone(),
        window: limits.budget.window,
        window_source: limits.window_source.clone(),
        estimated: true,
        estimate_total,
        breakdown,
        output_reserve: limits.budget.output_reserve,
        margin: limits.budget.margin,
        input_budget: limits.budget.input_budget,
        percent: percent(estimate_total, limits.budget.window),
        last_usage,
        skills: limits.skills,
        rule_files: limits.rule_files,
        incarnation,
        revision,
    }
}

/// What a session knows about its model for budgeting: identity and bounds
/// from the saved resolution, fixed for the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLimits {
    pub alias: String,
    pub gateway: String,
    pub upstream_id: String,
    pub variant: Option<String>,
    /// Where the window came from: `models.json` or a `discovered.*` field.
    pub window_source: String,
    pub budget: Budget,
    /// What the skills catalog holds; fixed with the session's skills snapshot.
    pub skills: SkillCounts,
    /// The rule files in the rules message; fixed with the rules snapshot.
    pub rule_files: u64,
}

/// The estimate as a share of the window, rounded to a whole percent.
pub fn percent(total: u64, window: u64) -> u64 {
    if window == 0 {
        return 0;
    }
    ((total as u128 * 100 + window as u128 / 2) / window as u128) as u64
}

/// A session's snapshot and what it needs to move to the next one: the
/// limits, the last provider usage and a revision counter. Pure state; the
/// runtime owns when to call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextTracker {
    limits: ContextLimits,
    incarnation: ulid::Ulid,
    last_usage: Option<LastUsage>,
    current: ContextSnapshot,
}

impl ContextTracker {
    /// The first snapshot (revision 0) of a session, from the estimate of the
    /// request its history would produce now.
    pub fn new(limits: ContextLimits, breakdown: Breakdown) -> Self {
        let incarnation = ulid::Ulid::generate();
        let current = context_snapshot(&limits, breakdown, None, incarnation, 0);
        Self {
            limits,
            incarnation,
            last_usage: None,
            current,
        }
    }

    /// The latest snapshot.
    pub fn snapshot(&self) -> &ContextSnapshot {
        &self.current
    }

    /// Remembers what the provider counted for the step `step`. It shows in
    /// the next snapshot [`ContextTracker::update`] produces.
    pub fn record_usage(&mut self, step: ulid::Ulid, usage: crate::provider::Usage) {
        self.last_usage = Some(LastUsage {
            model: format!("{}/{}", self.limits.gateway, self.limits.upstream_id),
            gateway: self.limits.gateway.clone(),
            upstream_id: self.limits.upstream_id.clone(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            step,
        });
    }

    /// Names the variant the session now runs with. Returns the new snapshot
    /// when the name changed (the revision goes up), `None` when it did not.
    pub fn set_variant(&mut self, variant: Option<String>) -> Option<&ContextSnapshot> {
        if self.limits.variant == variant {
            return None;
        }
        self.limits.variant = variant;
        self.current = context_snapshot(
            &self.limits,
            self.current.breakdown,
            self.last_usage.clone(),
            self.incarnation,
            self.current.revision + 1,
        );
        Some(&self.current)
    }

    /// Moves to a new estimate. Returns the new snapshot when the estimate or
    /// the last usage changed (the revision goes up), `None` when nothing did.
    pub fn update(&mut self, breakdown: Breakdown) -> Option<&ContextSnapshot> {
        if self.current.breakdown == breakdown && self.current.last_usage == self.last_usage {
            return None;
        }
        self.current = context_snapshot(
            &self.limits,
            breakdown,
            self.last_usage.clone(),
            self.incarnation,
            self.current.revision + 1,
        );
        Some(&self.current)
    }
}

#[cfg(test)]
mod tests;
