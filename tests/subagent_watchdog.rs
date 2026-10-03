use futures::future::BoxFuture;
use navai::session::subagents::watchdog::{Clock, Evidence, Judge, JudgeError, Verdict};
use navai::session::SessionId;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
mod common;
use common::deadline::Deadline;
use common::server::TestServerConfig;
use common::sse::{spawn_streaming_provider, text_chunk, tool_call_chunk};

struct FakeClock {
    now: Mutex<Instant>,
    sleeps: mpsc::UnboundedSender<oneshot::Sender<()>>,
}
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap()
    }
    fn sleep(&self, _: Duration) -> BoxFuture<'static, ()> {
        let (tx, rx) = oneshot::channel();
        self.sleeps.send(tx).unwrap();
        Box::pin(async move {
            let _ = rx.await;
        })
    }
}
struct FakeJudge {
    answers: Mutex<std::collections::VecDeque<Result<Verdict, JudgeError>>>,
    checked: mpsc::UnboundedSender<Evidence>,
}
impl Judge for FakeJudge {
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
struct Fixture {
    _tmp: tempfile::TempDir,
    service: navai::server::AppState,
    parent: SessionId,
    child: SessionId,
    provider: common::sse::StreamingProvider,
    stream: mpsc::UnboundedSender<String>,
    clock: Arc<FakeClock>,
    sleeps: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    checked: mpsc::UnboundedReceiver<Evidence>,
}
impl Fixture {
    async fn new(answers: Vec<Result<Verdict, JudgeError>>) -> Self {
        let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        let service = TestServerConfig::mock(tmp.path(), &provider.url)
            .chat_tools(128000)
            .state();
        let (tx, sleeps) = mpsc::unbounded_channel();
        let clock = Arc::new(FakeClock {
            now: Mutex::new(Instant::now()),
            sleeps: tx,
        });
        let (tx, checked) = mpsc::unbounded_channel();
        service.install_watchdog_test(
            clock.clone(),
            Arc::new(FakeJudge {
                answers: Mutex::new(answers.into()),
                checked: tx,
            }),
        );
        let parent = service
            .create_session(&ws, None, None)
            .bounded(file!(), line!())
            .await
            .unwrap();
        let child = service
            .create_child_session(parent, "explore", "inspect parser")
            .bounded(file!(), line!())
            .await
            .unwrap();
        let (_, stream) = provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        Self {
            _tmp: tmp,
            service,
            parent,
            child,
            provider,
            stream,
            clock,
            sleeps,
            checked,
        }
    }
    async fn tick(&mut self, seconds: u64) {
        let waiter = self.sleeps.recv().bounded(file!(), line!()).await.unwrap();
        *self.clock.now.lock().unwrap() += Duration::from_secs(seconds);
        waiter.send(()).unwrap();
    }
    async fn events(&self) -> Vec<navai::bus::event::EventEnvelope> {
        self.service
            .history()
            .flush()
            .bounded(file!(), line!())
            .await
            .unwrap();
        self.service
            .history()
            .replay(self.parent, None, u64::MAX)
            .bounded(file!(), line!())
            .await
            .unwrap()
    }
    async fn wait_verdict(&self, verdict: &str) -> Value {
        async {
            loop {
                for event in self.events().await {
                    if event.kind == "subagent.watchdog" && event.payload["verdict"] == verdict {
                        return event.payload;
                    }
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        .bounded(file!(), line!())
        .await
    }
    async fn close(&self) {
        self.service.close_all().bounded(file!(), line!()).await;
    }
}

#[tokio::test]
async fn active_records_replayable_verdict_without_waking_or_stopping() {
    let mut f = Fixture::new(vec![Ok(Verdict::Active), Ok(Verdict::Active)]).await;
    f.tick(900).await;
    let evidence = f.checked.recv().bounded(file!(), line!()).await.unwrap();
    assert_eq!(evidence.goal, "inspect parser");
    assert_eq!(evidence.step_count, 1);
    let payload = f.wait_verdict("active").await;
    assert_eq!(payload["child_session_id"], f.child.to_string());
    assert!(payload["checked_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        f.service
            .child_metadata(f.child)
            .bounded(file!(), line!())
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert!(f.provider.requests.try_recv().is_err());
    let waiter = f.sleeps.recv().bounded(file!(), line!()).await.unwrap();
    assert!(f.checked.try_recv().is_err(), "missed ticks were discarded");
    *f.clock.now.lock().unwrap() += Duration::from_secs(300);
    waiter.send(()).unwrap();
    f.checked.recv().bounded(file!(), line!()).await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn stuck_stops_with_partial_output_and_wakes_parent() {
    let mut f = Fixture::new(vec![Ok(Verdict::Stuck)]).await;
    let mut subscription = f
        .service
        .subscribe(f.child, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    f.stream
        .send(text_chunk("partial parser findings"))
        .unwrap();
    // Live deltas are not replayed from SQLite.
    async {
        loop {
            if subscription
                .live
                .as_mut()
                .unwrap()
                .recv()
                .await
                .unwrap()
                .kind
                == "llm.delta"
            {
                break;
            }
        }
    }
    .bounded(file!(), line!())
    .await;
    f.tick(300).await;
    f.wait_verdict("stuck").await;
    let (request, parent_stream) = f
        .provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(request.contains("stopped by watchdog"));
    assert!(request.contains("partial parser findings"));
    let link = f
        .service
        .child_metadata(f.child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.status, "stopped");
    assert_eq!(link.terminal_reason.as_deref(), Some("watchdog"));
    drop(parent_stream);
    f.close().await;
}

#[tokio::test]
async fn doubt_holds_next_request_and_model_decision_continues_same_turn() {
    let mut f = Fixture::new(vec![Ok(Verdict::Doubt)]).await;
    f.tick(300).await;
    f.wait_verdict("doubt").await;
    let (parent_request, parent_stream) = f
        .provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(parent_request.contains("may be stuck: continue or stop it?"));
    assert!(parent_request.contains("since_last_tool_call_secs"));
    assert!(parent_request.contains("decide_subagent"));
    f.stream
        .send(tool_call_chunk("glob", "glob", &json!({"pattern":"*"})))
        .unwrap();
    drop(f.stream);
    // Child tool completion proves the safe boundary is reached, without another provider request.
    async {
        loop {
            f.service.history().flush().await.unwrap();
            if f.service
                .history()
                .replay(f.child, None, u64::MAX)
                .await
                .unwrap()
                .iter()
                .any(|e| e.kind == "tool.post_call")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    }
    .bounded(file!(), line!())
    .await;
    assert!(f.provider.requests.try_recv().is_err());
    parent_stream
        .send(tool_call_chunk(
            "decision",
            "decide_subagent",
            &json!({"child_id":f.child,"decision":"continue","note":"keep inspecting"}),
        ))
        .unwrap();
    drop(parent_stream);
    let mut child_request = None;
    for _ in 0..2 {
        let (request, stream) = f
            .provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        if request.contains("keep inspecting") {
            child_request = Some(request);
        }
        drop(stream);
    }
    assert!(child_request.unwrap().contains("\"role\":\"tool\""));
    f.service.close_all().bounded(file!(), line!()).await;
}

#[tokio::test]
async fn one_failure_is_silent_two_failures_park_and_stop_decision_delivers_partial() {
    let mut f = Fixture::new(vec![Err(JudgeError::Unavailable), Err(JudgeError::Invalid)]).await;
    f.tick(300).await;
    f.checked.recv().bounded(file!(), line!()).await.unwrap();
    let waiter = f.sleeps.recv().bounded(file!(), line!()).await.unwrap();
    assert!(!f
        .events()
        .await
        .iter()
        .any(|e| e.kind == "subagent.watchdog"));
    assert!(f.provider.requests.try_recv().is_err());
    *f.clock.now.lock().unwrap() += Duration::from_secs(300);
    waiter.send(()).unwrap();
    assert_eq!(f.wait_verdict("doubt").await["reason"], "invalid");
    let (_, parent_stream) = f
        .provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    f.service
        .decide_subagent(f.parent, f.child, true, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let link = f
        .service
        .child_metadata(f.child)
        .bounded(file!(), line!())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.terminal_reason.as_deref(), Some("watchdog"));
    assert!(link.result_text.unwrap().contains("stopped by watchdog"));
    drop(parent_stream);
    f.close().await;
}

#[tokio::test]
async fn ownership_wrong_state_and_root_only_schema_are_enforced() {
    let f = Fixture::new(vec![]).await;
    let foreign = f
        .service
        .create_session(&f._tmp.path().join("ws"), None, None)
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(f
        .service
        .decide_subagent(foreign, f.child, false, None)
        .bounded(file!(), line!())
        .await
        .unwrap_err()
        .to_string()
        .contains("read-only"));
    assert!(f
        .service
        .decide_subagent(f.child, f.child, false, None)
        .bounded(file!(), line!())
        .await
        .is_err());
    assert!(f
        .service
        .decide_subagent(f.parent, f.child, false, None)
        .bounded(file!(), line!())
        .await
        .unwrap_err()
        .to_string()
        .contains("watchdog doubt"));
    // The child's first provider request has no root-only decision schema.
    assert!(f
        .service
        .continue_child(f.parent, f.child)
        .bounded(file!(), line!())
        .await
        .is_err());
    f.close().await;
}
