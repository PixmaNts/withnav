//! The Instinct judge compaction asks which old tool outputs still matter
//! (see `jev.rs`): one typed System One request, answered in one batch.

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::provider::systemone::{Answers, DecisionRequest, Questions, SystemOneClient};

/// The state and typed questions of one judge request.
#[derive(Debug, Clone)]
pub struct JudgeInput {
    pub state: String,
    pub questions: Questions,
}

pub trait Judge: Send + Sync {
    fn judge<'a>(
        &'a self,
        input: &'a JudgeInput,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Answers, String>>;
}

/// The session's Instinct layer as a judge.
pub struct InstinctJudge(pub SystemOneClient);

impl Judge for InstinctJudge {
    fn judge<'a>(
        &'a self,
        input: &'a JudgeInput,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Answers, String>> {
        Box::pin(async move {
            self.0
                .decide(
                    &DecisionRequest {
                        state: &input.state,
                        questions: &input.questions,
                    },
                    cancel,
                )
                .await
                .map(|r| r.answers)
                .map_err(|_| "unavailable".into())
        })
    }
}
