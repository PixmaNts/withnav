//! Deterministic clocks and judges for root and subagent watchdog integration tests.

use futures::future::BoxFuture;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use withnav::session::subagents::watchdog::{Clock, Evidence, Judge, JudgeError, Verdict};

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

impl Judge for ScriptedJudge {
    fn judge<'a>(
        &'a self,
        evidence: &'a Evidence,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Verdict, JudgeError>> {
        Box::pin(async move {
            self.checked.send(evidence.clone()).unwrap();
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted verdict")
        })
    }
}

pub struct ConstantJudge(pub Verdict);

impl Judge for ConstantJudge {
    fn judge<'a>(
        &'a self,
        _: &'a Evidence,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Verdict, JudgeError>> {
        Box::pin(async { Ok(self.0) })
    }
}
