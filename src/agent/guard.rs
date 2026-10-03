//! The role guard: a second opinion on what a role-bound agent is about to
//! do, asked of Jev (the System One model behind the `instinct` alias) before
//! the driver is.
//!
//! The `plan` agent keeps every tool, and its prompt says it only writes
//! markdown, researches and delegates. A `write` or `edit` of a markdown file
//! needs no question. Every other `write`, `edit` and every `bash` or `fetch`
//! is an exception: [`RoleGuard::judge`] is asked one typed question,
//! `role_conformance`, and only a valid `conforms` lets the call run without
//! the driver. Anything else (`violates`, `uncertain`, an answer that is not
//! one of the three options, no guard, a timeout, an input too large to send)
//! makes the call an ordinary permission request to the driver, whose reason
//! says why. The guard never blocks a call by itself: a "no" is a question to
//! the driver, not a refusal.
//!
//! The guard is ADVISORY, not a sandbox. It is a small fast model reading a
//! description of the call, `bash` can do anything the process can, and the
//! description it reads comes from the model it judges. It exists to catch an
//! agent drifting out of its role, not to confine a hostile one.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::provider::systemone::{
    Answer, DecisionRequest, Question, Questions, SystemOneClient, SYSTEMONE_DEADLINE,
};

/// The name of the one question the guard asks.
pub const QUESTION: &str = "role_conformance";

/// The most the guard sends, instructions and state together, in bytes. A
/// call whose description does not fit goes straight to the driver: a verdict
/// on a cut description would judge something else than the call.
pub const MAX_INPUT_BYTES: usize = 16 * 1024;

/// How much of the driver's latest message the guard reads, in bytes.
const MAX_TASK_BYTES: usize = 2 * 1024;

/// What Jev decided about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Conforms,
    Violates,
    Uncertain,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conforms => "conforms",
            Self::Violates => "violates",
            Self::Uncertain => "uncertain",
        }
    }
}

/// Why the guard gave no verdict.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    /// No guard is configured, or it could not be reached.
    #[error("{0}")]
    Unavailable(String),
    #[error("no answer within the deadline")]
    TimedOut,
    /// The answer was not a choice among the three options.
    #[error("{0}")]
    Malformed(String),
    /// The session was stopped while the guard was asked.
    #[error("canceled")]
    Cancelled,
}

/// What the guard reads about one call. Built by [`GuardInput::describe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardInput {
    /// The question's instructions.
    pub instructions: String,
    /// The description of the call.
    pub state: String,
}

/// The facts of one call the guard describes.
pub struct CallFacts<'a> {
    pub role_id: &'a str,
    pub role_prompt: &'a str,
    pub task: Option<&'a str>,
    pub workspace: &'a std::path::Path,
    pub tool: &'a str,
    /// The canonical file a `write` or `edit` resolved to.
    pub target: Option<&'a std::path::Path>,
    pub arguments: &'a Value,
}

impl GuardInput {
    /// Describes a call, or `None` when the description does not fit
    /// [`MAX_INPUT_BYTES`]: the arguments are sent exactly or not at all.
    pub fn describe(facts: &CallFacts<'_>) -> Option<Self> {
        let arguments = serde_json::to_string(facts.arguments).ok()?;
        let task = facts
            .task
            .map(|task| crate::tools::bound_output(task, MAX_TASK_BYTES).0)
            .unwrap_or_else(|| "(none)".to_string());
        let instructions = format!(
            "An agent bound to the role `{}` is about to run a tool call. Does the call \
             conform to that role, given the role instructions and the task the driver \
             gave? Answer `conforms` only when the role instructions clearly allow it.",
            facts.role_id
        );
        let target = facts
            .target
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none)".to_string());
        let state = format!(
            "Role instructions:\n{}\n\nCurrent task (the driver's latest message):\n{}\n\n\
             Workspace: {}\nWorking directory: {}\n\nTool: {}\nResolved target: {}\n\
             Arguments (exact):\n{}\n",
            facts.role_prompt,
            task,
            facts.workspace.display(),
            facts.workspace.display(),
            facts.tool,
            target,
            arguments,
        );
        let input = Self {
            instructions,
            state,
        };
        (input.size() <= MAX_INPUT_BYTES).then_some(input)
    }

    /// The bytes the guard would send.
    pub fn size(&self) -> usize {
        self.instructions.len() + self.state.len()
    }

    fn questions(&self) -> Questions {
        let criteria: BTreeMap<String, String> = [
            (
                "conforms",
                "The call is within the role: it does what the role instructions allow and \
                 serves the task.",
            ),
            (
                "violates",
                "The call does what the role instructions forbid: it changes code, \
                 configuration or the machine, or implements instead of planning.",
            ),
            (
                "uncertain",
                "The description does not allow a safe judgement either way.",
            ),
        ]
        .into_iter()
        .map(|(option, when)| (option.to_string(), when.to_string()))
        .collect();
        let mut questions = Questions::new();
        questions.insert(
            QUESTION.to_string(),
            Question::Choice {
                instructions: self.instructions.clone(),
                criteria,
            },
        );
        questions
    }
}

/// A second opinion on a call. Implemented over System One in production and
/// by fakes in tests.
pub trait RoleGuard: Send + Sync {
    fn judge<'a>(
        &'a self,
        input: &'a GuardInput,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Verdict, GuardError>>;
}

/// Jev, through `SystemOneClient` and its ~2 s deadline.
pub struct JevGuard(pub SystemOneClient);

impl RoleGuard for JevGuard {
    fn judge<'a>(
        &'a self,
        input: &'a GuardInput,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Verdict, GuardError>> {
        Box::pin(async move {
            let questions = input.questions();
            let response = self
                .0
                .decide(
                    &DecisionRequest {
                        state: &input.state,
                        questions: &questions,
                    },
                    cancel,
                )
                .await
                .map_err(provider_error)?;
            match response.answers.get(QUESTION) {
                Some(Answer::Choice { choice, .. }) => match choice.as_str() {
                    "conforms" => Ok(Verdict::Conforms),
                    "violates" => Ok(Verdict::Violates),
                    "uncertain" => Ok(Verdict::Uncertain),
                    other => Err(GuardError::Malformed(format!(
                        "`{other}` is not one of the three options"
                    ))),
                },
                Some(_) => Err(GuardError::Malformed(
                    "the answer is not a choice".to_string(),
                )),
                None => Err(GuardError::Malformed(
                    "the answer does not name the question".to_string(),
                )),
            }
        })
    }
}

fn provider_error(error: ProviderError) -> GuardError {
    match error {
        ProviderError::Canceled { .. } => GuardError::Cancelled,
        ProviderError::Api {
            status: 0,
            ref message,
            ..
        } if message.contains("deadline") => GuardError::TimedOut,
        ProviderError::Http(ref e) if e.is_timeout() => GuardError::TimedOut,
        ProviderError::MalformedResponse(message) => GuardError::Malformed(message),
        other => GuardError::Unavailable(other.to_string()),
    }
}

/// What asking the guard about a call came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Judged {
    /// A valid `conforms`: the call runs, with this note on its result.
    Allowed(Value),
    /// The driver decides, for this reason.
    AskDriver(String),
    /// The session was stopped meanwhile.
    Interrupted,
}

/// Asks `guard` about a call, under the deadline and the session's
/// cancellation. `input` is `None` when the description was too large.
pub async fn consult(
    guard: Option<&Arc<dyn RoleGuard>>,
    input: Option<GuardInput>,
    role_id: &str,
    cancel: &CancellationToken,
) -> Judged {
    let Some(input) = input else {
        return Judged::AskDriver(
            "The call is too large for Jev to judge, so the driver decides".to_string(),
        );
    };
    let Some(guard) = guard else {
        return Judged::AskDriver("Jev is not available".to_string());
    };
    let verdict = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Judged::Interrupted,
        verdict = tokio::time::timeout(
            SYSTEMONE_DEADLINE + std::time::Duration::from_millis(500),
            guard.judge(&input, cancel),
        ) => verdict,
    };
    match verdict {
        Ok(Ok(Verdict::Conforms)) => Judged::Allowed(json!({
            "by": "jev",
            "verdict": Verdict::Conforms.as_str(),
            "role": role_id,
        })),
        Ok(Ok(Verdict::Violates)) => {
            Judged::AskDriver(format!("Jev judged this outside the {role_id} role"))
        }
        Ok(Ok(Verdict::Uncertain)) => Judged::AskDriver(format!(
            "Jev could not tell whether this fits the {role_id} role"
        )),
        Ok(Err(GuardError::Cancelled)) => Judged::Interrupted,
        Ok(Err(GuardError::TimedOut)) | Err(_) => {
            Judged::AskDriver("Jev did not answer in time".to_string())
        }
        Ok(Err(GuardError::Unavailable(why))) => {
            Judged::AskDriver(format!("Jev is not available ({why})"))
        }
        Ok(Err(GuardError::Malformed(why))) => {
            Judged::AskDriver(format!("Jev gave an answer that cannot be used ({why})"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn facts<'a>(arguments: &'a Value, prompt: &'a str) -> CallFacts<'a> {
        CallFacts {
            role_id: "plan",
            role_prompt: prompt,
            task: Some("plan the parser"),
            workspace: Path::new("/w"),
            tool: "write",
            target: Some(Path::new("/w/src/main.rs")),
            arguments,
        }
    }

    #[test]
    fn the_description_carries_everything_the_spec_names() {
        let args = json!({"path": "/w/src/main.rs", "content": "fn main() {}"});
        let input = GuardInput::describe(&facts(&args, "plan only")).unwrap();
        for part in [
            "plan only",
            "plan the parser",
            "Workspace: /w",
            "Working directory: /w",
            "Tool: write",
            "Resolved target: /w/src/main.rs",
            &args.to_string(),
        ] {
            assert!(input.state.contains(part), "{part}: {}", input.state);
        }
        assert!(input.size() <= MAX_INPUT_BYTES);
        let Question::Choice { criteria, .. } = &input.questions()[QUESTION] else {
            panic!("a choice");
        };
        assert_eq!(
            criteria.keys().map(String::as_str).collect::<Vec<_>>(),
            ["conforms", "uncertain", "violates"]
        );
    }

    #[test]
    fn an_oversized_description_is_not_sent_cut() {
        let big = json!({"content": "x".repeat(MAX_INPUT_BYTES)});
        assert_eq!(GuardInput::describe(&facts(&big, "plan only")), None);
    }

    #[test]
    fn the_task_is_bounded_but_the_arguments_are_exact() {
        let args = json!({"command": "ls"});
        let mut f = facts(&args, "p");
        let long = "t".repeat(10_000);
        f.task = Some(&long);
        let input = GuardInput::describe(&f).unwrap();
        assert!(input.size() < 4 * 1024);
        assert!(input.state.contains(&args.to_string()));
    }

    #[test]
    fn provider_failures_map_to_guard_errors() {
        let deadline = ProviderError::Api {
            status: 0,
            message: "deadline exceeded (2000 ms)".into(),
            partial: None,
        };
        assert_eq!(provider_error(deadline), GuardError::TimedOut);
        assert_eq!(
            provider_error(ProviderError::Canceled { partial: None }),
            GuardError::Cancelled
        );
        assert!(matches!(
            provider_error(ProviderError::MalformedResponse("x".into())),
            GuardError::Malformed(_)
        ));
        assert!(matches!(
            provider_error(ProviderError::MissingApiKey),
            GuardError::Unavailable(_)
        ));
    }
}
