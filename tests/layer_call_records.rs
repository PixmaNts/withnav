#![cfg(feature = "test-support")]
//! The Instinct calls made outside flows (watchdogs, compaction pruning) leave one line each in
//! `<data dir>/instinct-calls.jsonl`: who asked, for which session, how big the state was, what
//! came back and how long it took, never the state or the question text.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use sha2::Digest;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use withnav::layers::recorded::{Caller, Recorded};
use withnav::provider::systemone::{
    Answer, Answers, DecisionError, DecisionModel, Question, Questions, ScriptedDecisionModel,
};
use withnav::session::subagents::watchdog::Verdict;

mod common;
use common::deadline::Deadline;
use common::server::TestServerConfig;
use common::sse::{spawn_streaming_provider, text_chunk};
use common::watchdog::{ConstantJudge, FakeClock};

const STATE: &str = r#"{"untrusted_evidence":"cat secrets.txt -> hunter2-PRIVATE-OUTPUT"}"#;
const INSTRUCTIONS: &str = "Is this tool output useful? output: hunter2-PRIVATE-QUESTION";

fn questions() -> Questions {
    [(
        "useful_t0".to_owned(),
        Question::Noul {
            instructions: INSTRUCTIONS.into(),
        },
    )]
    .into()
}

fn answers() -> Answers {
    [("useful_t0".to_owned(), Answer::Noul { noul: 0.25 })].into()
}

fn lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn recorded(inner: Arc<dyn DecisionModel>, log: &std::path::Path, model: Option<&str>) -> Recorded {
    Recorded::new(
        inner,
        model.map(str::to_owned),
        Caller::Prune,
        "01SESSION".into(),
        log.to_path_buf(),
    )
}

#[tokio::test]
async fn a_judgement_is_recorded_with_its_model_answers_and_latency() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("data").join("instinct-calls.jsonl");
    let model = recorded(
        Arc::new(ScriptedDecisionModel::new([Ok(answers())])),
        &log,
        Some("typesafe/jev-1.13"),
    );

    let result = model
        .decide(STATE, &questions(), &CancellationToken::new())
        .await;

    assert_eq!(result, Ok(answers()), "the answer passes through unchanged");
    let lines = lines(&log);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert!(line["ts"].as_str().is_some_and(|ts| ts.contains('T')));
    assert_eq!(line["caller"], "prune");
    assert_eq!(line["session_id"], "01SESSION");
    assert_eq!(line["model"], "typesafe/jev-1.13");
    assert_eq!(line["state_bytes"], STATE.len());
    assert_eq!(
        line["state_sha256"],
        hex::encode(sha2::Sha256::digest(STATE.as_bytes()))
    );
    assert_eq!(line["questions"], json!(["useful_t0"]));
    assert_eq!(
        line["answers"],
        json!({"useful_t0":{"type":"noul","noul":0.25}})
    );
    assert!(line["latency_ms"].is_u64());
    assert_eq!(line["error"], Value::Null);
}

#[tokio::test]
async fn a_failed_call_records_its_error_kind() {
    for (error, kind) in [
        (DecisionError::Unavailable, "unavailable"),
        (DecisionError::Invalid, "invalid"),
        (DecisionError::Cancelled, "cancelled"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("instinct-calls.jsonl");
        let model = recorded(
            Arc::new(ScriptedDecisionModel::new([Err(error)])),
            &log,
            None,
        );

        let result = model
            .decide(STATE, &questions(), &CancellationToken::new())
            .await;

        assert_eq!(result, Err(error), "the error passes through unchanged");
        let lines = lines(&log);
        assert_eq!(lines.len(), 1, "{kind}");
        assert_eq!(lines[0]["error"], kind);
        assert_eq!(lines[0]["answers"], Value::Null);
        assert_eq!(lines[0]["model"], Value::Null);
        assert!(lines[0]["latency_ms"].is_u64());
    }
}

/// Never answers: the caller's own deadline is what ends the call.
struct Silent;
impl DecisionModel for Silent {
    fn decide<'a>(
        &'a self,
        _: &'a str,
        _: &'a Questions,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Answers, DecisionError>> {
        Box::pin(futures::future::pending())
    }
}

#[tokio::test]
async fn a_call_dropped_before_its_answer_is_recorded_as_abandoned() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("instinct-calls.jsonl");
    let model = recorded(Arc::new(Silent), &log, Some("m"));

    let cancel = CancellationToken::new();
    let timed_out = tokio::time::timeout(
        Duration::from_millis(20),
        model.decide(STATE, &questions(), &cancel),
    )
    .await;

    assert!(timed_out.is_err());
    let lines = lines(&log);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["error"], "abandoned");
    assert_eq!(lines[0]["answers"], Value::Null);
    assert!(lines[0]["latency_ms"].as_u64().is_some_and(|ms| ms >= 20));
}

#[tokio::test]
async fn the_record_never_holds_the_state_text() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("instinct-calls.jsonl");
    for result in [Ok(answers()), Err(DecisionError::Unavailable)] {
        let model = recorded(
            Arc::new(ScriptedDecisionModel::new([result])),
            &log,
            Some("m"),
        );
        let _ = model
            .decide(STATE, &questions(), &CancellationToken::new())
            .await;
    }

    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("untrusted_evidence"), "{text}");
    assert!(!text.contains("Is this tool output useful"), "{text}");
}

#[tokio::test]
async fn an_unwritable_log_never_changes_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("a-file");
    std::fs::write(&blocker, "").unwrap();
    let log = blocker.join("instinct-calls.jsonl");
    let model = recorded(
        Arc::new(ScriptedDecisionModel::new([Ok(answers())])),
        &log,
        Some("m"),
    );

    let result = model
        .decide(STATE, &questions(), &CancellationToken::new())
        .await;

    assert_eq!(result, Ok(answers()));
    assert!(!log.exists());
}

#[tokio::test]
async fn a_child_watchdog_judgement_is_recorded_for_the_child_session() {
    let mut provider = spawn_streaming_provider().bounded(file!(), line!()).await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join(".withnav/agents")).unwrap();
    std::fs::write(
        ws.join(".withnav/agents/writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite.",
    )
    .unwrap();
    let service = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(128000)
        .state();
    let (tx, mut sleeps) = mpsc::unbounded_channel();
    let clock = Arc::new(FakeClock {
        now: Mutex::new(Instant::now()),
        sleeps: tx,
    });
    service.install_watchdog_test(clock.clone(), Arc::new(ConstantJudge(Verdict::Active)));
    let parent = service
        .create_session(&ws, None, false)
        .bounded(file!(), line!())
        .await
        .unwrap();
    let child = service
        .create_child_session(parent, "writer", "write the parser")
        .bounded(file!(), line!())
        .await
        .unwrap();
    let (_, stream) = provider
        .requests
        .recv()
        .bounded(file!(), line!())
        .await
        .unwrap();
    let tick = sleeps.recv().bounded(file!(), line!()).await.unwrap();
    *clock.now.lock().unwrap() += Duration::from_secs(300);
    tick.send(()).unwrap();

    let log = withnav::paths::instinct_call_log(&tmp.path().join("data"));
    let line = common::deadline::eventually("the child watchdog record", || {
        let log = log.clone();
        async move { lines(&log).into_iter().next() }
    })
    .await;

    assert_eq!(line["caller"], "child_watchdog");
    assert_eq!(line["session_id"], child.to_string());
    assert_eq!(line["error"], Value::Null);
    assert_eq!(line["answers"]["subagent_progress"]["choice"], "active");
    assert!(line["latency_ms"].is_u64());
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        !text.contains("write the parser"),
        "the goal is state: {text}"
    );
    stream.send(text_chunk("done")).unwrap();
    drop(stream);
    service.close_all().bounded(file!(), line!()).await;
}
