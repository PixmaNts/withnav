//! The session `Service` driven directly: no router, no HTTP.

use std::time::Duration;

use rust_agent::bus::ResumeOutcome;
use rust_agent::session::Service;

mod common;
use common::server::TestServerConfig;

// Given a Service over a provider nothing listens on
// When a session is created without any router
// Then no turn runs (no provider call), and a subscriber sees exactly
//      `session.start` at seq 0 and nothing after it
#[tokio::test]
async fn service_create_without_http_emits_start_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("workspace");
    let service = Service::new(TestServerConfig::unreachable(tmp.path()).config());

    let id = service
        .create_session(&ws, None, None)
        .await
        .expect("create without http");

    let handle = service.handle(id).expect("session is live");
    let mut reply = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("subscribe");
    let ResumeOutcome::Replay(events) = &reply.replay else {
        panic!("a fresh subscription replays: {:?}", reply.replay);
    };
    assert_eq!(events.len(), 1, "only session.start exists: {events:?}");
    assert_eq!(events[0].seq, 0);
    assert_eq!(events[0].kind.as_str(), "session.start");

    // A provider call would start a turn and emit more events.
    let mut live = reply.live.take().expect("live receiver");
    let late = tokio::time::timeout(Duration::from_millis(300), live.recv()).await;
    assert!(late.is_err(), "nothing follows session.start: {late:?}");
}
