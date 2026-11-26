#![cfg(feature = "test-support")]
//! The child watchdog's computed facts: counts WithNav folds from the child's own events and puts
//! in the question, so a looping child is told from a long-running one. The decision model here is
//! a scripted stand-in that answers from those counts, which proves they reach it and that the
//! verdict follows its answers to the repetition and progress questions.

use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use withnav::provider::systemone::{Answer, Answers, DecisionError, DecisionModel, Questions};
use withnav::session::SessionId;
mod common;
use common::deadline::Deadline;
use common::server::TestServerConfig;
use common::sse::{spawn_streaming_provider, tool_call_chunk};
use common::watchdog::FakeClock;

/// What the stand-in model was asked, and how it answers from the counts it was given.
struct CountingModel {
    states: Mutex<Vec<Value>>,
    questions: Mutex<Vec<Vec<String>>>,
    answer: fn(&Value) -> (&'static str, f64, f64),
}

impl DecisionModel for CountingModel {
    fn decide<'a>(
        &'a self,
        state: &'a str,
        questions: &'a Questions,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Answers, DecisionError>> {
        Box::pin(async move {
            let state: Value = serde_json::from_str(state).unwrap();
            let (verdict, same_strategy, progress) = (self.answer)(&state);
            self.states.lock().unwrap().push(state);
            self.questions
                .lock()
                .unwrap()
                .push(questions.keys().cloned().collect());
            Ok([
                (
                    "subagent_progress".to_owned(),
                    Answer::Choice {
                        choice: verdict.into(),
                        probabilities: Default::default(),
                        confidence: 1.0,
                    },
                ),
                (
                    "same_strategy".to_owned(),
                    Answer::Noul {
                        noul: same_strategy,
                    },
                ),
                ("progress".to_owned(), Answer::Noul { noul: progress }),
            ]
            .into_iter()
            .collect())
        })
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    service: withnav::server::AppState,
    parent: SessionId,
    child: SessionId,
    provider: common::sse::StreamingProvider,
    stream: mpsc::UnboundedSender<String>,
    clock: Arc<FakeClock>,
    sleeps: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    model: Arc<CountingModel>,
}

impl Fixture {
    async fn new(answer: fn(&Value) -> (&'static str, f64, f64)) -> Self {
        let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        let service = TestServerConfig::mock(tmp.path(), &provider.url)
            .chat_tools(128000)
            .allow_everything()
            .state();
        let (tx, sleeps) = mpsc::unbounded_channel();
        let clock = Arc::new(FakeClock {
            now: Mutex::new(Instant::now()),
            sleeps: tx,
        });
        let model = Arc::new(CountingModel {
            states: Mutex::default(),
            questions: Mutex::default(),
            answer,
        });
        service.install_watchdog_test(clock.clone(), model.clone());
        let parent = service
            .create_session(&ws, None, false)
            .bounded(file!(), line!())
            .await
            .unwrap();
        let child = service
            .create_child_session(parent, "mousse", "make the build pass")
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
            model,
        }
    }

    /// The child runs `command` through `bash` and waits for the model's next request, so the
    /// call's events are stored before anything is asked.
    async fn run(&mut self, id: &str, command: &str) {
        // Closing the response ends the model's turn, so the call runs.
        let (unused, _) = mpsc::unbounded_channel();
        let stream = std::mem::replace(&mut self.stream, unused);
        stream
            .send(tool_call_chunk(id, "bash", &json!({ "command": command })))
            .unwrap();
        drop(stream);
        let (_, next) = self
            .provider
            .requests
            .recv()
            .bounded(file!(), line!())
            .await
            .unwrap();
        self.stream = next;
        async {
            loop {
                let settled = self
                    .service
                    .history()
                    .replay(self.child, None, u64::MAX)
                    .await
                    .unwrap()
                    .iter()
                    .any(|e| e.kind == "tool.post_call" && e.payload["tool_call_id"] == id);
                if settled {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        .bounded(file!(), line!())
        .await;
    }

    async fn tick(&mut self, seconds: u64) {
        let waiter = self.sleeps.recv().bounded(file!(), line!()).await.unwrap();
        *self.clock.now.lock().unwrap() += Duration::from_secs(seconds);
        waiter.send(()).unwrap();
    }

    async fn verdict(&self) -> Value {
        async {
            loop {
                let events = self
                    .service
                    .history()
                    .replay(self.parent, None, u64::MAX)
                    .await
                    .unwrap();
                if let Some(event) = events.iter().find(|e| e.kind == "subagent.watchdog") {
                    return event.payload.clone();
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        .bounded(file!(), line!())
        .await
    }

    async fn child_status(&self) -> String {
        self.service
            .child_metadata(self.child)
            .await
            .unwrap()
            .unwrap()
            .status
    }
}

fn counts(state: &Value) -> &Value {
    &state["untrusted_evidence"]["computed_counts"]
}

/// A model that answers like a careful one: stuck on a long streak, repetition when the same call
/// repeated, progress when the streak is short.
fn from_counts(state: &Value) -> (&'static str, f64, f64) {
    let streak = counts(state)["repeat_streak"].as_u64().unwrap_or(0);
    if streak >= 3 {
        ("stuck", 0.9, 0.1)
    } else {
        ("stuck", 0.9, 0.9)
    }
}

// Given a child that ran the same failing command three times with the same error
// When the watchdog checks it
// Then the model is shown repeat_streak 3 and 3 unanswered failures, the recent exit statuses and
//      the failed attempts, is asked the two extra questions beside the verdict, and the child is
//      judged stuck and stopped
#[tokio::test]
async fn the_same_failing_call_three_times_with_no_new_error_is_stuck() {
    let mut f = Fixture::new(from_counts).await;
    for n in 0..3 {
        f.run(&format!("c{n}"), "echo boom >&2; exit 3").await;
    }
    f.tick(300).await;
    let verdict = f.verdict().await;
    assert_eq!(verdict["verdict"], "stuck");
    let states = f.model.states.lock().unwrap().clone();
    let seen = counts(&states[0]);
    assert_eq!(seen["repeat_streak"], 3);
    assert_eq!(seen["failures_unanswered"], 3);
    assert_eq!(seen["read_only_streak"], 0);
    assert_eq!(seen["minutes_since_progress"], 5);
    assert_eq!(seen["recent_exits"], json!(["exit 3", "exit 3", "exit 3"]));
    let attempts = states[0]["untrusted_attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 3);
    assert_eq!(attempts[2]["call"], "echo boom >&2; exit 3");
    assert_eq!(attempts[2]["error"], "boom");
    assert_eq!(
        f.model.questions.lock().unwrap()[0],
        ["progress", "same_strategy", "subagent_progress"]
    );
    assert_eq!(verdict["computed_counts"]["repeat_streak"], 3);
    let stopped = async {
        while f.child_status().await != "stopped" {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    stopped.bounded(file!(), line!()).await;
    f.service.close_all().bounded(file!(), line!()).await;
}

// Given a child that ran the same command three times but whose error changed each time
// When the watchdog checks it and the model still calls it stuck
// Then repeat_streak is 1 (a changed error is new information), the model sees progress, the
//      verdict is doubt (a review, not a stop) and the child keeps running
#[tokio::test]
async fn a_child_whose_error_changes_is_not_stuck() {
    let mut f = Fixture::new(from_counts).await;
    let command = "echo x >> n; echo error `wc -l < n` >&2; exit 3";
    for n in 0..3 {
        f.run(&format!("c{n}"), command).await;
    }
    f.tick(300).await;
    let verdict = f.verdict().await;
    assert_eq!(verdict["verdict"], "doubt");
    let states = f.model.states.lock().unwrap().clone();
    assert_eq!(counts(&states[0])["repeat_streak"], 1);
    assert_eq!(counts(&states[0])["failures_unanswered"], 1);
    assert_eq!(f.child_status().await, "running");
    f.service.close_all().bounded(file!(), line!()).await;
}

// Given a child that has made no tool call for an hour (one long-running tool, say) and a model
//      that calls it stuck on the elapsed time alone
// When the watchdog checks it
// Then nothing repeats and nothing failed, so the verdict is doubt and the child keeps running
#[tokio::test]
async fn a_long_running_single_tool_is_not_stuck_on_time_alone() {
    let mut f = Fixture::new(|state| {
        let idle = state["untrusted_evidence"]["since_last_tool_call_secs"]
            .as_u64()
            .unwrap_or(0);
        if idle >= 1800 {
            ("stuck", 0.1, 0.1)
        } else {
            ("active", 0.1, 0.9)
        }
    })
    .await;
    f.run("c0", "true").await;
    f.tick(3600).await;
    let verdict = f.verdict().await;
    assert_eq!(verdict["verdict"], "doubt");
    let states = f.model.states.lock().unwrap().clone();
    assert_eq!(counts(&states[0])["repeat_streak"], 1);
    assert_eq!(counts(&states[0])["failures_unanswered"], 0);
    assert_eq!(counts(&states[0])["minutes_since_progress"], 0);
    assert_eq!(f.child_status().await, "running");
    f.service.close_all().bounded(file!(), line!()).await;
}

// Given a model that answers only the verdict, as before the extra questions existed
// When it calls the child stuck
// Then the verdict stands: a model that does not answer a question is not read as disagreeing
#[tokio::test]
async fn a_verdict_without_the_extra_answers_keeps_its_verdict() {
    use common::watchdog::ConstantJudge;
    use withnav::session::subagents::watchdog::Verdict;
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .allow_everything()
        .state();
    let (tx, mut sleeps) = mpsc::unbounded_channel();
    let clock = Arc::new(FakeClock {
        now: Mutex::new(Instant::now()),
        sleeps: tx,
    });
    service.install_watchdog_test(clock.clone(), Arc::new(ConstantJudge(Verdict::Stuck)));
    let parent = service.create_session(&ws, None, false).await.unwrap();
    let child = service
        .create_child_session(parent, "vigie", "inspect")
        .await
        .unwrap();
    let (_, _stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let waiter = sleeps.recv().bounded(file!(), line!()).await.unwrap();
    *clock.now.lock().unwrap() += Duration::from_secs(300);
    waiter.send(()).unwrap();
    async {
        loop {
            let link = service.child_metadata(child).await.unwrap().unwrap();
            if link.status == "stopped" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    .bounded(file!(), line!())
    .await;
    service.close_all().bounded(file!(), line!()).await;
}
