//! Editor flow delivery records retain the decision log without legacy evaluation fields.
use serde_json::{Value, json};
use std::sync::Arc;
mod common;
use common::navigator::{
    EditorFlowsExt, Nav, RemarkAnswer, ScriptedCommenter, ScriptedDecision, URI,
    with_recorded_decision, with_recorded_flow_client,
};
fn triggers(nav: &Nav) -> Vec<Value> {
    nav.log_text()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("invalid log JSON"))
        .filter(|r| r["kind"] == "trigger")
        .collect()
}
#[tokio::test]
async fn flow_remark_keeps_delivery_log_and_omits_legacy_engine_provenance() {
    let mut nav = Nav::start(ScriptedCommenter::new(vec![RemarkAnswer::Remark(
        1,
        "observation",
    )]))
    .await;
    nav.open_and_edit().await;
    nav.did_save(URI).await;
    nav.expect_status("idle").await;
    let records = triggers(&nav);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["requested_action"], "remark");
    assert_eq!(record["delivered"], "remark");
    assert_eq!(record["prefilter"]["outcome"], "pass");
    for key in [
        "pipeline_hash",
        "branch",
        "questions",
        "answers",
        "rendered_state",
        "template_inputs",
    ] {
        assert!(record.get(key).is_none(), "{key}: {record}");
    }
}
#[tokio::test]
async fn failed_flow_records_failure_and_preserves_changed_lines() {
    let model = Arc::new(ScriptedDecision::failing("bad answer"));
    let mut nav =
        Nav::start_as_is(ScriptedCommenter::default(), |d, _| d.with_decision(model)).await;
    nav.open_and_edit().await;
    for _ in 0..2 {
        nav.did_save(URI).await;
        nav.expect_status("error").await;
    }
    let records = triggers(&nav);
    assert_eq!(records.len(), 2);
    for record in records {
        assert_eq!(record["reason"], "flow");
        assert_eq!(record["delivered"], "provider_error");
        assert_eq!(record["prefilter"]["inputs"]["changed_lines"], 4);
    }
}
#[tokio::test]
async fn no_flow_factory_leaves_the_editor_trigger_unhandled() {
    let model = ScriptedCommenter::default();
    let mut nav = Nav::start_as_is(model.clone(), |d, _| d).await;
    nav.open_and_edit().await;
    nav.did_save(URI).await;
    nav.sync_notifications().await;
    assert_eq!(triggers(&nav), Vec::<Value>::new());
    assert_eq!(model.calls(), 0);
}
#[tokio::test]
async fn skipped_builtin_flow_records_no_action() {
    let mut nav = Nav::start(ScriptedCommenter::default()).await;
    nav.open_base().await;
    nav.did_save(URI).await;
    nav.expect_status("idle").await;
    let records = triggers(&nav);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["reason"], "flow_skipped");
    assert_eq!(records[0]["delivered"], "none");
    assert_eq!(records[0].get("requested_action"), None);
    assert_eq!(records[0]["prefilter"]["inputs"]["changed_lines"], json!(0));
}

/// The run ids of the flow run records, in capture order.
fn captured_run_ids(records: &withnav::flows::records::Records) -> Vec<String> {
    records.shutdown(std::time::Duration::from_secs(2));
    records
        .read()
        .unwrap()
        .into_iter()
        .map(|record| record.run_id)
        .collect()
}

// The Instinct answers of a flow trigger live in the flow run record, not in
// the trigger record. Each trigger names the runs it started so the two logs
// join on the run id, for a delivered remark and a silent run. A skipped run
// writes no run record, so its trigger names none.
#[tokio::test]
async fn flow_trigger_records_name_the_flow_runs_they_started() {
    for (answer, delivered) in [
        (RemarkAnswer::Remark(1, "observation"), "remark"),
        (RemarkAnswer::Silent, "silent"),
    ] {
        let mut recorded = None;
        let mut nav = Nav::start_as_is(ScriptedCommenter::new(vec![answer]), |deps, _| {
            let (deps, records) = with_recorded_decision(
                deps,
                Arc::new(ScriptedDecision::routing_to_metacognition()),
            );
            recorded = Some(records);
            deps
        })
        .await;
        nav.open_and_edit().await;
        nav.did_save(URI).await;
        nav.expect_status("idle").await;
        let records = triggers(&nav);
        assert_eq!(records.len(), 1, "{delivered}");
        assert_eq!(records[0]["delivered"], delivered);
        let named: Vec<String> =
            serde_json::from_value(records[0]["flow_run_ids"].clone()).expect("flow_run_ids");
        let captured = captured_run_ids(&recorded.unwrap());
        assert!(!named.is_empty(), "{delivered}: {}", records[0]);
        for run_id in &named {
            assert!(
                captured.contains(run_id),
                "{delivered}: {run_id} not in {captured:?}"
            );
        }
        assert!(
            records[0].get("answers").is_none(),
            "answers stay in the run record"
        );
    }
}

#[tokio::test]
async fn a_skipped_flow_trigger_names_no_run() {
    let mut recorded = None;
    let mut nav = Nav::start_as_is(ScriptedCommenter::default(), |deps, _| {
        let (deps, records) =
            with_recorded_decision(deps, Arc::new(ScriptedDecision::routing_to_metacognition()));
        recorded = Some(records);
        deps
    })
    .await;
    nav.open_base().await;
    nav.did_save(URI).await;
    nav.expect_status("idle").await;
    let records = triggers(&nav);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["reason"], "flow_skipped");
    assert!(records[0].get("flow_run_ids").is_none(), "{}", records[0]);
    assert_eq!(captured_run_ids(&recorded.unwrap()), Vec::<String>::new());
}

// A remark that ran out of its budget judged nothing. Recording it as
// silence rebased the document, so the unjudged change was consumed and the
// next trigger saw no diff. It is logged as over budget, the diff stays, and
// the next save judges the same change.
#[tokio::test]
async fn an_over_budget_remark_leaves_the_change_for_the_next_trigger() {
    let commenter = ScriptedCommenter::new(vec![
        RemarkAnswer::OverBudget,
        RemarkAnswer::Remark(1, "observation"),
    ]);
    let mut nav = Nav::start(commenter.clone()).await;
    nav.open_and_edit().await;
    for _ in 0..2 {
        nav.did_save(URI).await;
        // The remark delivery reports idle, then the flow evaluation ends
        // with idle once the document is free for the next trigger.
        nav.expect_status("idle").await;
        nav.expect_status("idle").await;
    }
    let records = triggers(&nav);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["requested_action"], "remark");
    assert_eq!(records[0]["delivered"], "none");
    assert_eq!(records[0]["reason"], "over_budget");
    assert_eq!(records[1]["prefilter"]["inputs"]["changed_lines"], 4);
    assert_eq!(records[1]["delivered"], "remark");
    assert_eq!(commenter.calls(), 2);
    let log: Vec<Value> = nav
        .log_text()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        !log.iter()
            .any(|r| r["kind"] == "silent" || r["kind"] == "error"),
        "a budget timeout is neither silence nor a provider error: {log:?}"
    );
    let calls: Vec<&Value> = log
        .iter()
        .filter(|r| r["kind"] == "call" && r["layer"] == "metacognition")
        .collect();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0]["cancelled"], true, "the timed-out call");
    assert_eq!(calls[1]["cancelled"], false);
}

// The remark call record is the cost line of the metacognition: it carries
// the tokens the provider reported for the remark, for a remark and for a
// silence alike, and leaves them out when the provider reported none.
#[tokio::test]
async fn remark_call_records_carry_the_reported_tokens() {
    let usage = withnav::provider::Usage {
        prompt_tokens: 812,
        completion_tokens: 23,
        total_tokens: 835,
    };
    for (answer, reported, delivered) in [
        (
            RemarkAnswer::Remark(1, "observation"),
            Some(usage),
            "remark",
        ),
        (RemarkAnswer::Silent, Some(usage), "silent"),
        (RemarkAnswer::Remark(1, "observation"), None, "remark"),
        // Rounds completed before the budget ran out or a round failed
        // still cost tokens, and the call record says so.
        (RemarkAnswer::OverBudget, Some(usage), "none"),
        (RemarkAnswer::Fail("boom"), Some(usage), "provider_error"),
    ] {
        let delivery_status = if delivered == "provider_error" {
            "error"
        } else {
            "idle"
        };
        let mut commenter = ScriptedCommenter::new(vec![answer]);
        if let Some(usage) = reported {
            commenter = commenter.with_usage(usage);
        }
        let mut nav = Nav::start(commenter).await;
        nav.open_and_edit().await;
        nav.did_save(URI).await;
        nav.expect_status(delivery_status).await;
        nav.expect_status("idle").await;
        assert_eq!(triggers(&nav)[0]["delivered"], delivered);
        let call: Value = nav
            .log_text()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|r| r["kind"] == "call" && r["layer"] == "metacognition")
            .expect("a metacognition call record");
        match reported {
            Some(_) => {
                assert_eq!(call["input_tokens"], 812, "{delivered}: {call}");
                assert_eq!(call["output_tokens"], 23, "{delivered}: {call}");
            }
            None => {
                assert!(call.get("input_tokens").is_none(), "{call}");
                assert!(call.get("output_tokens").is_none(), "{call}");
            }
        }
    }
}

/// A flow decision client whose every call is refused by the provider.
struct RejectingInstinct;
impl instinct_flow::DecisionClient for RejectingInstinct {
    fn capabilities(&self) -> instinct_flow::Capabilities {
        instinct_flow::Capabilities { images: false }
    }
    fn identity(&self) -> instinct_flow::ClientIdentity {
        instinct_flow::ClientIdentity {
            model: "test/rejected".into(),
            variant: None,
        }
    }
    fn decide<'a>(
        &'a self,
        _: instinct_flow::protocol::DecisionRequest<'a>,
        _: &'a tokio_util::sync::CancellationToken,
        _: std::time::Instant,
    ) -> instinct_flow::BoxFuture<
        'a,
        Result<instinct_flow::protocol::DecisionResponse, instinct_flow::DecisionError>,
    > {
        Box::pin(async {
            Err(instinct_flow::DecisionError::ProviderRejected {
                status: 400,
                reason: Some("model jev-router is not served".into()),
            })
        })
    }
}

// A failed flow run is the one a Captain most needs to look up: its trigger
// names it, and its run record holds the provider's reason.
#[tokio::test]
async fn a_failed_flow_trigger_names_the_run_that_holds_the_rejection() {
    let mut recorded = None;
    let mut nav = Nav::start_as_is(ScriptedCommenter::default(), |deps, _| {
        let (deps, records) = with_recorded_flow_client(deps, Arc::new(RejectingInstinct));
        recorded = Some(records);
        deps
    })
    .await;
    nav.open_and_edit().await;
    nav.did_save(URI).await;
    nav.expect_status("error").await;
    let records = triggers(&nav);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["delivered"], "provider_error");
    assert_eq!(records[0]["reason"], "flow");
    let named: Vec<String> =
        serde_json::from_value(records[0]["flow_run_ids"].clone()).expect("flow_run_ids");
    assert!(!named.is_empty(), "{}", records[0]);
    let recorded = recorded.unwrap();
    recorded.shutdown(std::time::Duration::from_secs(2));
    let runs = recorded.read().unwrap();
    for run_id in &named {
        let run = runs
            .iter()
            .find(|run| &run.run_id == run_id)
            .unwrap_or_else(|| panic!("no run record {run_id}"));
        let failure = run.failure.as_ref().expect("a failed run");
        assert!(
            failure.message.contains(
                "provider rejected the request (HTTP 400): model jev-router is not served"
            ),
            "{}",
            failure.message
        );
    }
}
