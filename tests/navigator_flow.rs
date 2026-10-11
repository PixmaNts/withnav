//! The navigator's flow of ordinary remarks: which triggers reach the
//! metacognition, and how a remark in flight is treated. Drives the real
//! server over an in-memory connection with scripted models.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use withnav::navigator::{Metacognition, ModelError, Remark, RemarkContext};

mod common;
use common::navigator::{
    big_edit, Nav, RemarkAnswer as Answer, ScriptedCommenter as MockCommenter, URI,
};

struct ObservedCommenter {
    inner: MockCommenter,
    started: tokio::sync::Semaphore,
}

impl ObservedCommenter {
    async fn wait_for_calls(&self, count: u32) {
        tokio::time::timeout(Duration::from_secs(20), self.started.acquire_many(count))
            .await
            .expect("metacognition calls started")
            .expect("metacognition call signal open")
            .forget();
    }
}

impl Metacognition for ObservedCommenter {
    fn remark<'a>(
        &'a self,
        ctx: &'a RemarkContext,
        cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<Option<Remark>, ModelError>> {
        Box::pin(async move {
            self.started.add_permits(1);
            self.inner.remark(ctx, cancel).await
        })
    }
}

/// The trigger records of the log, in order.
fn triggers(client: &Nav) -> Vec<Value> {
    client
        .log_text()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["kind"] == "trigger")
        .collect()
}

/// A save then an idle on the same text: one request, one delivery. The idle
/// does not restart the request the save started.
#[tokio::test(start_paused = true)]
async fn a_save_then_an_idle_on_the_same_text_make_one_request() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(
        vec![Answer::Remark(1, "first"), Answer::Remark(1, "second")],
        gate.clone(),
    );
    let mut client = Nav::start(mock.clone()).await;
    client.open_and_edit().await;
    client.did_save(URI).await;
    client.expect_status("thinking").await;

    tokio::time::advance(Duration::from_millis(3001)).await;
    client.sync_notifications().await;
    assert_eq!(mock.calls(), 1, "the idle must not restart the request");

    gate.cancel();
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "first");
    let rest = client.collect_for(Duration::from_millis(100)).await;
    assert!(rest.iter().all(|m| m["method"] != "withnav/remark"));

    let records = triggers(&client);
    let delivered: Vec<&str> = records
        .iter()
        .map(|r| r["delivered"].as_str().unwrap())
        .collect();
    assert_eq!(delivered.iter().filter(|d| **d == "remark").count(), 1);
    assert!(!delivered.contains(&"cancelled"), "{delivered:?}");
    assert_eq!(records.len(), 1);
}

/// Newer text still supersedes the request in flight.
#[tokio::test]
async fn newer_text_still_supersedes_the_remark_in_flight() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(
        vec![Answer::Remark(0, "old"), Answer::Remark(0, "new")],
        gate.clone(),
    );
    let observed = Arc::new(ObservedCommenter {
        inner: mock.clone(),
        started: tokio::sync::Semaphore::new(0),
    });
    let mut client = Nav::start_with(mock.clone(), serde_json::json!({}), |mut deps, _| {
        deps.commenter = observed.clone();
        deps
    })
    .await;
    client.open_and_edit().await;
    client.did_save(URI).await;
    client.expect_status("thinking").await;

    client
        .did_change(URI, &format!("{}\n// tail\n", big_edit()), 3)
        .await;
    client.did_save(URI).await;
    observed.wait_for_calls(2).await;
    assert_eq!(mock.calls(), 2, "the newer text asked for its own remark");

    gate.cancel();
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "new");
    assert!(triggers(&client)
        .iter()
        .any(|r| r["delivered"] == "cancelled"));
}

/// Closing the document cancels the remark in flight, deduplication or not.
#[tokio::test]
async fn closing_cancels_the_deduplicated_remark() {
    let gate = Arc::new(CancellationToken::new());
    let mock = MockCommenter::gated(vec![Answer::Remark(0, "late")], gate.clone());
    let mut client = Nav::start(mock.clone()).await;
    client.open_and_edit().await;
    client.did_save(URI).await;
    client.expect_status("thinking").await;
    client.did_save(URI).await;
    client
        .notify(
            "textDocument/didClose",
            serde_json::json!({"textDocument": {"uri": URI}}),
        )
        .await;
    let mut seen = Vec::new();
    client.expect("withnav/clear", &mut seen).await;
    gate.cancel();
    let msgs = client.collect_for(Duration::from_millis(100)).await;
    assert!(msgs.iter().all(|m| m["method"] != "withnav/remark"));
    assert!(triggers(&client)
        .iter()
        .any(|r| r["delivered"] == "cancelled"));
    assert_eq!(mock.calls(), 1);
}

/// A one-line replacement reaches the metacognition under the built-in
/// pipeline, with no size limit to clear.
#[tokio::test]
async fn a_one_line_replacement_reaches_the_metacognition() {
    let mock = MockCommenter::new(vec![Answer::Remark(1, "x")]);
    let mut client = Nav::start(mock.clone()).await;
    client.open_base().await;
    client
        .did_change(URI, "fn main() {\n    println!(\"hi\");\n}", 2)
        .await;
    client.did_save(URI).await;
    let mut seen = Vec::new();
    let remark = client.expect("withnav/remark", &mut seen).await;
    assert_eq!(remark["params"]["text"], "x");
    assert_eq!(mock.calls(), 1);
}
