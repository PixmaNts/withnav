//! The context budget: what the next request will weigh, by category, and how
//! much of the model's window it may fill.
//!
//! Everything here is pure. The estimate is a size guess (UTF-8 bytes / 4,
//! rounded up, plus a fixed framing per message), not a tokenizer: it is
//! labelled an estimate wherever it is shown, and it is never mixed with the
//! provider's own usage numbers.

use crate::agent::prompt::SYSTEM_CONTRACT;
use crate::provider::openai::{ChatMessage, ToolDef};

/// Tokens added to every message for its role and delimiters.
pub const MESSAGE_FRAMING_TOKENS: u64 = 4;
/// Output reserve cap when `models.json` sets no output limit.
pub const DEFAULT_OUTPUT_RESERVE: u64 = 8192;
/// Smallest safety margin, in tokens.
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
        ChatMessage::System { content } | ChatMessage::User { content } => content.len(),
        ChatMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
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
        } => tool_call_id.len() + content.len(),
    };
    bytes_tokens(bytes) + MESSAGE_FRAMING_TOKENS
}

/// The tool definitions as they are serialized into the request.
pub fn tools_tokens(tools: &[ToolDef]) -> u64 {
    let bytes: usize = tools
        .iter()
        .map(|t| serde_json::to_string(t).map_or(0, |s| s.len()))
        .sum();
    bytes_tokens(bytes)
}

/// The estimated size of one request, split by where its text comes from.
/// The categories add up to [`Breakdown::total`] exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Breakdown {
    /// The system contract and the role prompt.
    pub system: u64,
    /// The local instructions message.
    pub rules: u64,
    /// The skills catalog message.
    pub skills_catalog: u64,
    /// The serialized tool definitions.
    pub tools: u64,
    /// The history: user and assistant text, reasoning, tool calls, tool
    /// outputs and loaded skill bodies.
    pub messages: u64,
}

impl Breakdown {
    pub fn total(&self) -> u64 {
        self.system + self.rules + self.skills_catalog + self.tools + self.messages
    }
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
        system: framed(SYSTEM_CONTRACT) + system_prompt.map_or(0, framed),
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
            "; set a larger \"context_window\" or a smaller \"max_output_tokens\" for the alias in models.json"
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

/// M: 5% of the window, at least [`MIN_MARGIN`].
pub fn margin(window: u64) -> u64 {
    MIN_MARGIN.max(window.div_ceil(20))
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

/// What a session knows about its model for budgeting: identity and bounds
/// from the saved resolution, fixed for the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLimits {
    pub alias: String,
    pub variant: Option<String>,
    /// Where the window came from: `models.json` or a `discovered.*` field.
    pub window_source: String,
    pub budget: Budget,
}

/// The estimate as a share of the window, rounded to a whole percent.
pub fn percent(total: u64, window: u64) -> u64 {
    if window == 0 {
        return 0;
    }
    ((total as u128 * 100 + window as u128 / 2) / window as u128) as u64
}

#[cfg(test)]
mod tests;
