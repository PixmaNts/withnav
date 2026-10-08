//! Editor flow delivery records retain the decision log without legacy evaluation fields.
use serde_json::{json, Value};
use std::sync::Arc;
mod common;
use common::navigator::{
    EditorFlowsExt, Nav, RemarkAnswer, ScriptedCommenter, ScriptedDecision, URI,
};
fn triggers(nav: &Nav) -> Vec<Value> {
    nav.log_text()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
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
