//! Effect execution and generation tagging, with no terminal and no server:
//! the client points at a closed port, so every request fails fast and the
//! tagged result is what is observed.
use std::time::Duration;

use tokio::sync::mpsc;

use super::*;
use crate::tui::protocol::{CreateSessionRequest, Generation, SubmitRequest};

const DEAD_URL: &str = "http://127.0.0.1:1";

fn executor() -> (Executor, mpsc::Receiver<ClientEvent>) {
    let (tx, rx) = mpsc::channel(16);
    (Executor::new(Client::new(DEAD_URL, None), tx), rx)
}

fn ctx(generation: u64, session: Option<&str>) -> Context {
    Context {
        generation: Generation(generation),
        session_id: session.map(str::to_string),
        workspace: "/ws".into(),
        config_dir: None,
    }
}

async fn next(rx: &mut mpsc::Receiver<ClientEvent>) -> ClientEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("event in time")
        .expect("channel open")
}

fn submit() -> SubmitRequest {
    SubmitRequest {
        target_actor_id: "agent:main".into(),
        mode: "next-turn".into(),
        content: vec![],
    }
}

#[tokio::test]
async fn http_effects_report_back_tagged_with_the_context_generation() {
    let (mut exec, mut rx) = executor();
    let mut editor = Editor::new();
    let ctx = ctx(7, Some("s1"));
    for effect in [
        Effect::FetchModels,
        Effect::ListSessions,
        Effect::Create(CreateSessionRequest {
            workspace: String::new(),
            model: None,
            resume: None,
        }),
        Effect::Resume("s1".into()),
        Effect::FetchMetadata("s1".into()),
        Effect::Stop("s1".into()),
    ] {
        assert_eq!(exec.execute(effect, &ctx, &mut editor), Flow::Continue);
    }
    let mut seen = Vec::new();
    for _ in 0..6 {
        let (generation, name) = match next(&mut rx).await {
            ClientEvent::Models { generation, result } => {
                assert!(result.is_err());
                (generation, "models")
            }
            ClientEvent::Sessions { generation, result } => {
                assert!(result.is_err());
                (generation, "sessions")
            }
            ClientEvent::Created { generation, result } => {
                assert!(result.is_err());
                (generation, "created")
            }
            ClientEvent::Resumed { generation, result } => {
                assert!(result.is_err());
                (generation, "resumed")
            }
            ClientEvent::Metadata { generation, result } => {
                assert!(result.is_err());
                (generation, "metadata")
            }
            ClientEvent::Stopped { generation, result } => {
                assert!(result.is_err());
                (generation, "stopped")
            }
            other => panic!("unexpected event {other:?}"),
        };
        assert_eq!(generation, Generation(7), "{name}");
        seen.push(name);
    }
    seen.sort_unstable();
    assert_eq!(
        seen,
        ["created", "metadata", "models", "resumed", "sessions", "stopped"]
    );
}

#[tokio::test]
async fn effects_use_the_generation_of_the_moment_they_run() {
    let (mut exec, mut rx) = executor();
    let mut editor = Editor::new();
    exec.execute(Effect::FetchModels, &ctx(1, None), &mut editor);
    exec.execute(Effect::FetchModels, &ctx(2, None), &mut editor);
    let mut generations = Vec::new();
    for _ in 0..2 {
        match next(&mut rx).await {
            ClientEvent::Models { generation, .. } => generations.push(generation.0),
            other => panic!("unexpected event {other:?}"),
        }
    }
    generations.sort_unstable();
    assert_eq!(generations, [1, 2]);
}

#[tokio::test]
async fn a_failed_send_keeps_the_action_revision_and_the_generation() {
    let (mut exec, mut rx) = executor();
    let mut editor = Editor::new();
    exec.execute(
        Effect::SendAt {
            request: submit(),
            revision: 42,
        },
        &ctx(3, Some("s1")),
        &mut editor,
    );
    match next(&mut rx).await {
        ClientEvent::Submitted {
            generation,
            revision,
            result,
        } => {
            assert_eq!((generation, revision), (Generation(3), 42));
            assert!(result.is_err());
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[tokio::test]
async fn send_without_a_conversation_fails_instead_of_vanishing() {
    let (mut exec, mut rx) = executor();
    let mut editor = Editor::new();
    exec.execute(
        Effect::SendAt {
            request: submit(),
            revision: 0,
        },
        &ctx(1, None),
        &mut editor,
    );
    assert!(matches!(
        next(&mut rx).await,
        ClientEvent::Submitted { result: Err(_), .. }
    ));
}

#[tokio::test]
async fn clear_editor_only_clears_an_unedited_draft() {
    let (mut exec, _rx) = executor();
    let mut editor = Editor::new();
    editor.set_text("hello");
    let sent = editor.revision();
    editor.set_text("hello world");
    exec.execute(
        Effect::ClearEditor { revision: sent },
        &ctx(1, None),
        &mut editor,
    );
    assert_eq!(editor.text(), "hello world");
    let current = editor.revision();
    exec.execute(
        Effect::ClearEditor { revision: current },
        &ctx(1, None),
        &mut editor,
    );
    assert_eq!(editor.text(), "");
}

#[tokio::test]
async fn quit_ends_the_loop() {
    let (mut exec, _rx) = executor();
    let mut editor = Editor::new();
    assert_eq!(
        exec.execute(Effect::Quit, &ctx(1, None), &mut editor),
        Flow::Quit
    );
}

#[tokio::test]
async fn subscribe_replaces_the_previous_stream_and_unsubscribe_cancels_it() {
    let (mut exec, _rx) = executor();
    let mut editor = Editor::new();
    assert!(!exec.has_stream());
    exec.execute(
        Effect::Subscribe {
            session_id: "a".into(),
            cursor: None,
        },
        &ctx(1, Some("a")),
        &mut editor,
    );
    assert!(exec.has_stream());
    exec.execute(
        Effect::Subscribe {
            session_id: "b".into(),
            cursor: Some(4),
        },
        &ctx(2, Some("b")),
        &mut editor,
    );
    assert!(exec.has_stream());
    exec.retire_stale_stream(&Generation(3));
    assert!(
        !exec.has_stream(),
        "a stream of an older generation is retired"
    );
    exec.execute(
        Effect::Subscribe {
            session_id: "c".into(),
            cursor: None,
        },
        &ctx(3, Some("c")),
        &mut editor,
    );
    exec.retire_stale_stream(&Generation(3));
    assert!(exec.has_stream(), "the current generation's stream stays");
    exec.execute(Effect::Unsubscribe, &ctx(3, Some("c")), &mut editor);
    assert!(!exec.has_stream());
}

fn options(model: Option<&str>) -> TuiOptions {
    TuiOptions {
        url: DEAD_URL.into(),
        token: None,
        workspace: "/ws".into(),
        initial_model: model.map(str::to_string),
        initial_session: None,
        embedded: false,
        config_dir: None,
    }
}

#[tokio::test]
async fn startup_presets_the_model_and_asks_for_catalog_and_sessions() {
    let mut runtime = Runtime::new(&options(Some("fast")));
    assert_eq!(runtime.state.selected_model_alias.as_deref(), Some("fast"));
    runtime.start(None);
    let mut kinds = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(Duration::from_secs(10), runtime.recv())
            .await
            .expect("in time")
            .expect("open");
        kinds.push(match event {
            ClientEvent::Models { .. } => "models",
            ClientEvent::Sessions { .. } => "sessions",
            other => panic!("unexpected event {other:?}"),
        });
    }
    kinds.sort_unstable();
    assert_eq!(kinds, ["models", "sessions"]);
}

#[tokio::test]
async fn a_protocol_error_event_is_kept_and_the_loop_goes_on() {
    let mut runtime = Runtime::new(&options(None));
    runtime.apply_batch(ClientEvent::ProtocolError {
        generation: runtime.state.generation.clone(),
        message: "bad frame".into(),
    });
    assert_eq!(
        runtime.state.last_protocol_error.as_deref(),
        Some("bad frame")
    );
    assert!(!runtime.should_quit());
}

#[tokio::test]
async fn a_batch_applies_at_most_the_cap_of_queued_events() {
    let mut runtime = Runtime::new(&options(None));
    let tx = runtime.exec_sender();
    for i in 0..(MAX_EVENTS_PER_ITERATION + 5) {
        tx.send(ClientEvent::ProtocolError {
            generation: runtime.state.generation.clone(),
            message: format!("e{i}"),
        })
        .await
        .unwrap();
    }
    let first = runtime.recv().await.unwrap();
    runtime.apply_batch(first);
    assert_eq!(
        runtime.state.last_protocol_error.as_deref(),
        Some(format!("e{}", MAX_EVENTS_PER_ITERATION - 1).as_str())
    );
    assert!(runtime.recv().await.is_some(), "the rest stays queued");
}

#[test]
fn a_rejected_send_is_a_failed_submission() {
    let event = effects::submit_event(
        Generation(5),
        9,
        Err(effects::SubmitFailure::Rejected("HTTP 400".into())),
    );
    assert_eq!(
        event,
        ClientEvent::Submitted {
            generation: Generation(5),
            revision: 9,
            result: Err("HTTP 400".into()),
        }
    );
}

#[test]
fn an_uncertain_send_is_reported_as_delivery_uncertain() {
    let event = effects::submit_event(
        Generation(5),
        9,
        Err(effects::SubmitFailure::Uncertain("timeout".into())),
    );
    assert_eq!(
        event,
        ClientEvent::DeliveryUncertain {
            generation: Generation(5)
        }
    );
}
