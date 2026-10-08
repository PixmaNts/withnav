//! Current remark state transitions over synthetic buffers and in-memory LSP.

use std::collections::BTreeMap;
use std::sync::Arc;

use withnav::bus::event::{EventEnvelope, EventKind};
use withnav::navigator::ports::Answer;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

mod common;
use common::navigator::EditorFlowsExt;
use common::navigator::{
    big_edit, feed_with_session, Nav, RemarkAnswer, ScriptedCommenter, ScriptedDecision, URI,
};

fn triggers(nav: &Nav) -> Vec<Value> {
    nav.log_text()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|record| record["kind"] == "trigger")
        .collect()
}

fn silent_answers() -> BTreeMap<String, Answer> {
    BTreeMap::from([
        ("worth_attention".into(), Answer::Noul { noul: 0.1 }),
        ("drift".into(), Answer::Noul { noul: 0.1 }),
        (
            "contradicts_conversation".into(),
            Answer::Noul { noul: 0.1 },
        ),
        ("unfinished".into(), Answer::Noul { noul: 0.1 }),
        ("stuck".into(), Answer::Noul { noul: 0.1 }),
        (
            "route".into(),
            Answer::Choice {
                choice: "silent".into(),
                probabilities: BTreeMap::from([("silent".into(), 1.0)]),
                confidence: 1.0,
            },
        ),
    ])
}

#[tokio::test]
async fn successful_instinct_silence_consumes_the_diff_without_starting_a_remark() {
    let model = Arc::new(ScriptedDecision::fixed(silent_answers()));
    let commenter = ScriptedCommenter::default();
    let mut nav = Nav::start_as_is(commenter.clone(), |deps, _| {
        deps.with_decision(model.clone())
    })
    .await;
    nav.open_and_edit().await;
    nav.did_save(URI).await;
    nav.expect_status("idle").await;
    nav.did_save(URI).await;
    nav.expect_status("idle").await;
    let records = triggers(&nav);
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["reason"], "flow_skipped");
    assert_eq!(records[0]["requested_action"], "silent");
    assert_eq!(records[0]["delivered"], "silent");
    assert_eq!(model.calls(), 1, "the second save has no unjudged diff");
    assert_eq!(commenter.calls(), 0);
}

#[tokio::test]
async fn failed_instinct_keeps_the_diff_for_the_next_save() {
    let model = Arc::new(ScriptedDecision::failing("synthetic malformed answer"));
    let commenter = ScriptedCommenter::default();
    let mut nav = Nav::start_as_is(commenter.clone(), |deps, _| {
        deps.with_decision(model.clone())
    })
    .await;
    nav.open_and_edit().await;
    for _ in 0..2 {
        nav.did_save(URI).await;
        nav.expect_status("error").await;
    }
    let records = triggers(&nav);
    assert_eq!(records.len(), 2);
    for record in records {
        assert_eq!(record["prefilter"]["inputs"]["changed_lines"], 4);
        assert_eq!(record["delivered"], "provider_error");
        assert_eq!(record["reason"], "flow");
        assert!(record.get("requested_action").is_none());
    }
    assert_eq!(model.calls(), 2);
    assert_eq!(commenter.calls(), 0);
}

#[tokio::test]
async fn newer_edit_cancels_cooperative_instinct_and_next_save_keeps_the_diff() {
    // This fake respects cancellation. Rejection of a late successful answer from
    // a non-cooperative client remains a separate evaluator acceptance boundary.
    let gate = Arc::new(CancellationToken::new());
    let mut stale = silent_answers();
    stale.insert("worth_attention".into(), Answer::Noul { noul: 0.95 });
    stale.insert(
        "route".into(),
        Answer::Choice {
            choice: "metacognition".into(),
            probabilities: BTreeMap::from([("metacognition".into(), 1.0)]),
            confidence: 1.0,
        },
    );
    let model = Arc::new(ScriptedDecision::gated(
        vec![stale, silent_answers()],
        gate.clone(),
    ));
    let commenter = ScriptedCommenter::default();
    let mut nav = Nav::start_as_is(commenter.clone(), |deps, _| {
        deps.with_decision(model.clone())
    })
    .await;
    nav.open_and_edit().await;
    nav.did_save(URI).await;
    model.wait_for_call().await;
    nav.expect_status("thinking").await;
    nav.did_change(URI, &format!("{}\n// synthetic tail", big_edit()), 3)
        .await;
    nav.sync_notifications().await;
    gate.cancel();
    nav.expect_status("idle").await;
    nav.did_save(URI).await;
    nav.expect_status("idle").await;
    let records = triggers(&nav);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["version"], 2);
    assert_eq!(records[0]["delivered"], "cancelled");
    assert_eq!(records[1]["version"], 3);
    assert_eq!(records[1]["prefilter"]["inputs"]["changed_lines"], 7);
    assert_eq!(records[1]["delivered"], "silent");
    assert_eq!(model.calls(), 2);
    assert_eq!(commenter.calls(), 0);
}

#[tokio::test]
async fn diagnostics_update_context_without_a_builtin_trigger_or_log_record() {
    let model = Arc::new(ScriptedDecision::fixed(silent_answers()));
    let mut nav = Nav::start_as_is(ScriptedCommenter::default(), |deps, _| {
        deps.with_decision(model.clone())
    })
    .await;
    nav.open_and_edit().await;
    nav.notify(
        "navigator/diagnostics",
        json!({
            "uri": URI,
            "items": [{"line": 1, "severity": "error", "message": "synthetic type error"}],
        }),
    )
    .await;
    let seen = nav.sync_notifications().await;
    assert!(seen.iter().all(|msg| msg["method"] != "navigator/remark"));
    assert_eq!(model.calls(), 0);
    assert!(triggers(&nav).is_empty());
    nav.did_save(URI).await;
    nav.expect_status("idle").await;
    let records = triggers(&nav);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["event"], "save");
    assert_eq!(records[0]["prefilter"]["inputs"]["has_errors"], true);
    assert!(model.states()[0].contains("synthetic type error"));
}

#[tokio::test]
async fn conversation_silence_logs_one_question_and_leaves_the_edit_for_save() {
    let (feed, _, session) = feed_with_session(Ok(())).await;
    let model = Arc::new(ScriptedDecision::new(vec![
        BTreeMap::from([(
            "code_contradicts_conversation".into(),
            Answer::Noul { noul: 0.1 },
        )]),
        silent_answers(),
    ]));
    let mut nav = Nav::start_as_is(ScriptedCommenter::default(), |deps, _| {
        deps.with_decision(model.clone())
            .with_conversation(feed.clone())
    })
    .await;
    nav.open_and_edit().await;
    nav.sync_notifications().await;
    feed.observe(&EventEnvelope::new(
        session,
        2,
        EventKind::TurnEnd,
        json!({"outcome": "done"}),
    ))
    .await;
    nav.expect_status("idle").await;
    nav.did_save(URI).await;
    nav.expect_status("idle").await;
    let records = triggers(&nav);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["event"], "conversation");
    assert_eq!(records[0]["requested_action"], "silent");
    assert_eq!(records[0]["delivered"], "silent");
    assert_eq!(model.questions()[0].len(), 1);
    assert!(model.states()[0].contains("driver:"));
    assert_eq!(records[1]["event"], "save");
    assert_eq!(records[1]["prefilter"]["inputs"]["changed_lines"], 4);
    assert_eq!(model.calls(), 2);
}

#[tokio::test]
async fn metacognition_silence_consumes_the_diff_but_failed_answers_do_not() {
    for (answer, status, delivered, calls) in [
        (RemarkAnswer::Silent, "idle", "silent", 1),
        (
            RemarkAnswer::Fail("synthetic outage"),
            "error",
            "provider_error",
            2,
        ),
        (
            RemarkAnswer::Malformed("synthetic invalid JSON"),
            "error",
            "malformed",
            2,
        ),
    ] {
        let model = Arc::new(ScriptedDecision::routing_to_metacognition());
        let commenter = ScriptedCommenter::new(vec![answer.clone(), answer]);
        let mut nav = Nav::start_as_is(commenter.clone(), |deps, _| {
            deps.with_decision(model.clone())
        })
        .await;
        nav.open_and_edit().await;
        for _ in 0..2 {
            nav.did_save(URI).await;
            nav.expect_status(status).await;
        }
        let records = triggers(&nav);
        assert_eq!(records.len(), calls, "{delivered}: {records:?}");
        for record in records {
            assert_eq!(record["requested_action"], "remark");
            assert_eq!(record["delivered"], delivered);
            assert_eq!(record["prefilter"]["inputs"]["changed_lines"], 4);
        }
        assert_eq!(model.calls(), calls, "{delivered}");
        assert_eq!(commenter.calls(), calls, "{delivered}");
    }
}
