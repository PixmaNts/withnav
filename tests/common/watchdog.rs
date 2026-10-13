//! Deterministic clocks and judges for root and subagent watchdog integration tests.

use futures::future::BoxFuture;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use withnav::session::subagents::watchdog::{Clock, Evidence, JudgeError, Verdict};

use withnav::provider::systemone::{Answer, Answers, DecisionError, DecisionModel, Questions};

pub struct FakeClock {
    pub now: Mutex<Instant>,
    pub sleeps: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap()
    }

    fn sleep(&self, _: Duration) -> BoxFuture<'static, ()> {
        let (tx, rx) = oneshot::channel();
        // A missing receiver is a harness error: fail instead of silently waking
        // the supervisor and allowing checks without the test's ordered tick.
        self.sleeps.send(tx).expect("watchdog sleep receiver");
        Box::pin(async move {
            let _ = rx.await;
        })
    }
}

pub struct ScriptedJudge {
    pub answers: Mutex<VecDeque<Result<Verdict, JudgeError>>>,
    pub checked: mpsc::UnboundedSender<Evidence>,
}

impl DecisionModel for ScriptedJudge {
    fn decide<'a>(
        &'a self,
        state: &'a str,
        _: &'a Questions,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Answers, DecisionError>> {
        Box::pin(async move {
            let value: serde_json::Value = serde_json::from_str(state).unwrap();
            self.checked
                .send(serde_json::from_value(value["untrusted_evidence"].clone()).unwrap())
                .unwrap();
            verdict_answer(
                self.answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("scripted verdict"),
            )
        })
    }
}

pub struct ConstantJudge(pub Verdict);

impl DecisionModel for ConstantJudge {
    fn decide<'a>(
        &'a self,
        _: &'a str,
        _: &'a Questions,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Answers, DecisionError>> {
        Box::pin(async { verdict_answer(Ok(self.0)) })
    }
}

pub fn verdict_answer(answer: Result<Verdict, JudgeError>) -> Result<Answers, DecisionError> {
    answer
        .map(|verdict| {
            [(
                "subagent_progress".into(),
                Answer::Choice {
                    choice: serde_json::to_value(verdict)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .into(),
                    probabilities: Default::default(),
                    confidence: 1.0,
                },
            )]
            .into_iter()
            .collect()
        })
        .map_err(|error| match error {
            JudgeError::Unavailable => DecisionError::Unavailable,
            JudgeError::Invalid => DecisionError::Invalid,
            JudgeError::Cancelled => DecisionError::Cancelled,
        })
}
