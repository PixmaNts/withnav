//! The summary step: one request to the session's own model and variant, no
//! tools, then the answer checked before it is trusted.

use std::collections::HashSet;

use tokio_util::sync::CancellationToken;

use super::{
    estimate_effective, loaded_skill_calls, plan, validate, Aim, Checkpoint, Compaction,
    CompactionError, Components, Plan, SUMMARY_MAX_BYTES,
};
use crate::agent::context::message_tokens;
use crate::error::ProviderError;
use crate::provider::client::ModelClient;
use crate::provider::config::{ProviderConfig, Variant};
use crate::provider::openai::{ChatMessage, ChatRequest, ClientCache, StreamOptions};
use crate::provider::AssistantMessage;

/// What the summarizing model is told it is doing.
pub const SUMMARY_PROMPT: &str = "\
You write the memory of a coding agent whose conversation no longer fits its context window. \
Below is the earlier part of that conversation (a transcript, possibly led by an earlier \
summary). Write a summary the agent will continue from; it will not see the transcript again.

Cover, in this order, as short sections:
1. Goal: what the user is trying to achieve.
2. Constraints: requirements, preferences and rules the user or the project set.
3. Decisions: what was decided and why.
4. Work done and verified: what changed, and which checks (tests, builds, commands) confirmed it.
5. Files: the paths that matter and what each holds or needs.
6. Open questions: what is unresolved or waiting on the user.
7. Next action: the single next step.
8. Skills to reload: the ids of skills that were loaded and are still needed (they are not kept).

Be concrete: exact paths, commands, names, error messages. Keep what is needed to continue; drop \
chit-chat and dead ends. Do not call tools. Do not address the user. Stay under 6000 characters. \
Answer with the summary only.";

/// Everything [`prepare`] needs about the session.
pub struct PrepareInput<'a> {
    pub provider: &'a ProviderConfig,
    pub api_key: String,
    pub model: &'a str,
    pub variant: Option<&'a Variant>,
    pub max_tokens: Option<u64>,
    /// The full transcript.
    pub transcript: &'a [ChatMessage],
    /// The checkpoint installed now, if any.
    pub current: Option<&'a Compaction>,
    pub components: Components<'a>,
    /// The input budget the prompt must fit afterwards: the session's own for
    /// `/compact`, the target model's for a switch.
    pub destination_budget: u64,
    /// How much to summarize.
    pub aim: Aim,
    /// The input budget of the session's own model: the summary request must
    /// fit it.
    pub source_budget: u64,
    /// What the user asked the summary to pay particular attention to.
    pub focus: Option<&'a str>,
}

/// A checkpoint ready to persist and install. Nothing is saved yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub compaction: Compaction,
    /// The estimated prompt before and after, on the destination's terms.
    pub estimated_before: u64,
    pub estimated_after: u64,
}

/// Plans, asks the model for the summary, validates it and checks that the
/// resulting prompt fits `destination_budget`. Persists nothing.
pub async fn prepare(
    input: PrepareInput<'_>,
    cancel: &CancellationToken,
) -> Result<Prepared, CompactionError> {
    let planned = plan(
        input.transcript,
        input.current,
        &input.components,
        input.destination_budget,
        input.aim,
    )?;
    let estimated_before =
        estimate_effective(&input.components, input.transcript, input.current).total();
    let request = build_summary_request(&input, &planned)?;
    let message = ask(&input, request, cancel).await?;
    let summary = validate_summary(&message)?;

    let compaction = Compaction {
        checkpoint: Checkpoint {
            through_message: planned.through_message,
            summary,
            generation: input.current.map_or(1, |c| c.checkpoint.generation + 1),
        },
        pruned_message_indices: planned.kept_pruned(),
    };
    validate(&compaction, input.transcript)?;
    let estimated_after =
        estimate_effective(&input.components, input.transcript, Some(&compaction)).total();
    if estimated_after > input.destination_budget {
        return Err(CompactionError::Capacity {
            needed: estimated_after,
            budget: input.destination_budget,
        });
    }
    Ok(Prepared {
        compaction,
        estimated_before,
        estimated_after,
    })
}

async fn ask(
    input: &PrepareInput<'_>,
    request: ChatRequest,
    cancel: &CancellationToken,
) -> Result<AssistantMessage, CompactionError> {
    let client = ModelClient::new(
        "agent",
        input.provider,
        input.api_key.clone(),
        &ClientCache::new(),
    )
    .map_err(|e| CompactionError::Provider(e.to_string()))?;
    client
        .chat(request, input.variant, cancel, &mut |_| {})
        .await
        .map_err(|e| match e {
            ProviderError::Canceled { .. } => CompactionError::Cancelled,
            other => CompactionError::Provider(other.to_string()),
        })
}

/// The summary request: the dedicated prompt, then the part to summarize as
/// one block of text. No tools. Outputs of that part are cut, biggest first,
/// only when the request would not fit the session's own budget.
pub fn build_summary_request(
    input: &PrepareInput<'_>,
    planned: &Plan,
) -> Result<ChatRequest, CompactionError> {
    let first = input
        .current
        .map_or(0, |c| c.checkpoint.through_message + 1);
    let part = &input.transcript[first..=planned.through_message];
    let mut elided: HashSet<usize> = input
        .current
        .map(|c| c.pruned_message_indices.iter().copied().collect())
        .unwrap_or_default();
    let system = ChatMessage::System {
        content: match input.focus.map(str::trim).filter(|f| !f.is_empty()) {
            Some(focus) => format!(
                "{SUMMARY_PROMPT}\n\nThe user asked you to pay particular attention to: {focus}"
            ),
            None => SUMMARY_PROMPT.to_string(),
        },
    };
    let mut cut = planned.prefix_elidable.iter();
    loop {
        let user = ChatMessage::User {
            content: render_part(
                input.current.map(|c| c.checkpoint.summary.as_str()),
                input.transcript,
                first,
                part.len(),
                &elided,
            ),
        };
        let needed = message_tokens(&system) + message_tokens(&user);
        if needed <= input.source_budget {
            return Ok(ChatRequest {
                model: input.model.to_string(),
                messages: vec![system, user],
                tools: Vec::new(),
                stream: true,
                stream_options: Some(StreamOptions {
                    include_usage: true,
                }),
                max_tokens: input.max_tokens,
                temperature: None,
                extra: Default::default(),
            });
        }
        match cut.next() {
            Some(&i) => {
                elided.insert(i);
            }
            None => {
                return Err(CompactionError::SummaryInputTooLarge {
                    needed,
                    budget: input.source_budget,
                })
            }
        }
    }
}

/// The part to summarize as plain text: tool exchanges are written out, so
/// the request needs no tool definitions.
fn render_part(
    earlier: Option<&str>,
    transcript: &[ChatMessage],
    first: usize,
    len: usize,
    elided: &HashSet<usize>,
) -> String {
    let mut out = String::new();
    if let Some(summary) = earlier {
        out.push_str("<earlier_summary>\n");
        out.push_str(summary);
        out.push_str("\n</earlier_summary>\n\n");
    }
    out.push_str("<transcript>\n");
    for (i, message) in transcript.iter().enumerate().skip(first).take(len) {
        match message {
            ChatMessage::System { content } => push(&mut out, "system", content),
            ChatMessage::User { content } => push(&mut out, "user", content),
            ChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if let Some(text) = content.as_deref().filter(|t| !t.is_empty()) {
                    push(&mut out, "assistant", text);
                }
                for call in tool_calls {
                    push(
                        &mut out,
                        &format!("assistant called {}", call.function.name),
                        &call.function.arguments,
                    );
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                let label = format!("tool result {tool_call_id}");
                if elided.contains(&i) {
                    push(&mut out, &label, &super::elision_marker(content.len()));
                } else {
                    push(&mut out, &label, content);
                }
            }
        }
    }
    out.push_str("</transcript>\n");
    let skills = loaded_skill_calls(&transcript[first..first + len]);
    if !skills.is_empty() {
        out.push_str("\nSkills loaded in this part: ");
        let names: Vec<&str> = skills.iter().map(|(_, n)| n.as_str()).collect();
        out.push_str(&names.join(", "));
        out.push('\n');
    }
    out
}

fn push(out: &mut String, label: &str, text: &str) {
    out.push_str(&format!("[{label}]\n{text}\n\n"));
}

/// Accepts the answer when it is a usable summary: non-empty, no tool call,
/// not cut off, at most [`SUMMARY_MAX_BYTES`].
pub fn validate_summary(message: &AssistantMessage) -> Result<String, CompactionError> {
    let bad = |why: &str| Err(CompactionError::InvalidSummary(why.to_string()));
    if !message.tool_calls.is_empty() {
        return bad("the model called a tool instead of answering");
    }
    if message.finish_reason.as_deref() == Some("length") {
        return bad("the answer was cut off by the output limit");
    }
    let text = message.text.trim();
    if text.is_empty() {
        return bad("the answer was empty");
    }
    if text.len() > SUMMARY_MAX_BYTES {
        return Err(CompactionError::InvalidSummary(format!(
            "the answer is {} bytes, over the limit of {SUMMARY_MAX_BYTES}",
            text.len()
        )));
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests;
