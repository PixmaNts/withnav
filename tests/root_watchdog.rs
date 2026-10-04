use futures::future::BoxFuture;
use navai::session::subagents::watchdog::{Clock, Evidence, Judge, JudgeError, Verdict};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
mod common;
use common::deadline::Deadline;
use common::server::{collect_until, send_text, TestServerConfig};
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
        let _ = self.sleeps.send(tx);
        Box::pin(async move {
            let _ = rx.await;
        })
    }
}
struct FakeJudge(Verdict);
impl Judge for FakeJudge {
    fn judge<'a>(
        &'a self,
        _: &'a Evidence,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Verdict, JudgeError>> {
        Box::pin(async { Ok(self.0) })
    }
}

#[tokio::test]
async fn root_doubt_parks_after_results_and_owner_continue_releases_the_same_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let mut provider = spawn_streaming_provider().await;
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let (tx, mut sleeps) = mpsc::unbounded_channel();
    let clock = Arc::new(FakeClock {
        now: Mutex::new(Instant::now()),
        sleeps: tx,
    });
    service.install_root_watchdog_test(clock.clone(), Arc::new(FakeJudge(Verdict::Doubt)), None);
    let id = service.create_session(&ws, None, None).await.unwrap();
    let sub = service.subscribe(id, None).await.unwrap();
    send_text(&service, id, "find files").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let tick = sleeps.recv().bounded(file!(), line!()).await.unwrap();
    *clock.now.lock().unwrap() += Duration::from_secs(300);
    tick.send(()).unwrap();
    let events = collect_until(sub, |event| {
        event.kind == "agent.yield" && event.payload["kind"] == "watchdog"
    })
    .await;
    assert!(events.iter().any(|event| event.payload["question"]
        .as_str()
        .is_some_and(|q| q.contains("alt+c"))));
    stream
        .send(tool_call_chunk(
            "glob",
            "glob",
            &serde_json::json!({"pattern":"*"}),
        ))
        .unwrap();
    drop(stream);
    let sub = service.subscribe(id, None).await.unwrap();
    collect_until(sub, |event| event.kind == "tool.post_call").await;
    assert!(provider.requests.try_recv().is_err());
    service.continue_child(id, id).await.unwrap();
    let (request, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    assert!(request.contains("\"role\":\"tool\""));
    stream.send(text_chunk("finished")).unwrap();
    drop(stream);
    let sub = service.subscribe(id, None).await.unwrap();
    let events = collect_until(sub, |event| event.kind == "turn.end").await;
    assert_eq!(events.iter().filter(|e| e.kind == "turn.start").count(), 1);
    service.close_all().await;
}

#[tokio::test]
async fn absent_instinct_caps_at_five_hundred_and_announces_once() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let provider = common::sse::spawn_responding_provider(501, |_, _| {
        common::sse::http_sse(&common::sse::sse_body(&[&tool_call_chunk(
            "glob",
            "glob",
            &serde_json::json!({"pattern":"*"}),
        )]))
    })
    .await;
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(1_000_000)
        .state();
    let id = service.create_session(&ws, None, None).await.unwrap();
    let sub = service.subscribe(id, None).await.unwrap();
    send_text(&service, id, "keep looking").await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    assert_eq!(
        events.iter().filter(|e| e.kind == "llm.response").count(),
        500
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.payload["name"] == "watchdog")
            .count(),
        1
    );
    assert!(events.iter().any(|e| e.payload["kind"] == "budget"));
    service.continue_child(id, id).await.unwrap();
    service.close_all().await;
}

#[tokio::test]
async fn stuck_root_settles_tool_results_without_requesting_another_step() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let mut provider = spawn_streaming_provider().await;
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let (tx, mut sleeps) = mpsc::unbounded_channel();
    let clock = Arc::new(FakeClock {
        now: Mutex::new(Instant::now()),
        sleeps: tx,
    });
    service.install_root_watchdog_test(clock.clone(), Arc::new(FakeJudge(Verdict::Stuck)), None);
    let id = service.create_session(&ws, None, None).await.unwrap();
    send_text(&service, id, "find files").await;
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let tick = sleeps.recv().bounded(file!(), line!()).await.unwrap();
    *clock.now.lock().unwrap() += Duration::from_secs(300);
    tick.send(()).unwrap();
    let sub = service.subscribe(id, None).await.unwrap();
    collect_until(sub, |e| e.payload["name"] == "watchdog").await;
    stream
        .send(tool_call_chunk(
            "glob",
            "glob",
            &serde_json::json!({"pattern":"*"}),
        ))
        .unwrap();
    drop(stream);
    let sub = service.subscribe(id, None).await.unwrap();
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    let post = events
        .iter()
        .position(|e| e.kind == "tool.post_call")
        .unwrap();
    let end = events.iter().position(|e| e.kind == "turn.end").unwrap();
    assert!(post < end);
    assert!(provider.requests.try_recv().is_err());
    assert!(events.iter().any(|e| e.payload["data"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("Stopped by watchdog"))));
    service.close_all().await;
}
