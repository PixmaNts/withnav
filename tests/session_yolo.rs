//! The `yolo` setting through the session service: read once when a root
//! session starts (or resumes), kept by its children and by model and agent
//! switches, and never reviving a permission request the process lost.

use std::time::Duration;

use serde_json::{Value, json};

use withnav::server::{AppState, app};

mod common;
use common::server::{TestServerConfig, captain_command, request_json, stored_events as events};
use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_chunk, tool_calls_chunk};

use common::TestEnv as Env;

fn state(env: &Env) -> AppState {
    TestServerConfig::unreachable(&env.root)
        .extra_alias("plan-key", "plan-model")
        .state()
}

/// Rewrites the settings file the next session reads.
fn settings(env: &Env, yolo: Option<bool>, agents: Value) {
    let mut document = json!({"agents": agents, "permissions": {"scratch_dirs": []}});
    if let Some(yolo) = yolo {
        document["yolo"] = json!(yolo);
    }
    std::fs::write(env.root.join("cfg/config.json"), document.to_string()).unwrap();
}

fn plain_agents() -> Value {
    json!({"build": {"model": "mock/mock-model"}, "plan": {"model": "mock/plan-model"}})
}

async fn yolo_of(state: &AppState, id: ulid::Ulid) -> bool {
    state.session_summary(id).await.unwrap().yolo
}

// Given a parent started with yolo on or off
// When the settings change and it spawns children, one of them with a model
//      assigned to its own agent
// Then each child keeps the parent's value, and a fresh root reads the new one
#[tokio::test]
async fn children_keep_the_parents_yolo_and_a_fresh_root_reads_the_new_setting() {
    for parent_yolo in [true, false] {
        let env = Env::new();
        let state = state(&env);
        settings(&env, Some(parent_yolo), plain_agents());
        let parent = state.create_session(&env.ws, None, false).await.unwrap();
        assert_eq!(yolo_of(&state, parent).await, parent_yolo);

        settings(&env, Some(!parent_yolo), plain_agents());
        let inheriting = state
            .create_child_session(parent, "vigie", "inspect")
            .await
            .unwrap();
        assert_eq!(yolo_of(&state, inheriting).await, parent_yolo);

        let mut assigned = plain_agents();
        assigned["vigie"] = json!({"model": "mock/plan-model"});
        settings(&env, Some(!parent_yolo), assigned);
        let own_model = state
            .create_child_session(parent, "vigie", "inspect with its own model")
            .await
            .unwrap();
        assert_eq!(
            state.session_summary(own_model).await.unwrap().model,
            "mock/plan-model",
            "the child really uses the assigned model"
        );
        assert_eq!(yolo_of(&state, own_model).await, parent_yolo);

        let fresh = state.create_session(&env.ws, None, false).await.unwrap();
        assert_eq!(yolo_of(&state, fresh).await, !parent_yolo);
        assert_eq!(yolo_of(&state, parent).await, parent_yolo, "no hot reload");
    }
}

// Given a live yolo session whose settings then turn it off
// When the model and then the agent are switched
// Then the session keeps the value it started with
#[tokio::test]
async fn model_and_agent_switches_keep_the_started_value() {
    for started in [true, false] {
        let env = Env::new();
        let state = state(&env);
        settings(&env, Some(started), plain_agents());
        let id = state.create_session(&env.ws, None, false).await.unwrap();
        settings(&env, Some(!started), plain_agents());
        let out = state
            .run_command(id, "model", "mock/plan-model")
            .await
            .unwrap();
        assert!(out.ok, "{out:?}");
        assert_eq!(yolo_of(&state, id).await, started);
        let out = state.run_command(id, "agent", "plan").await.unwrap();
        assert!(out.ok, "{out:?}");
        assert_eq!(yolo_of(&state, id).await, started);
    }
}

// Given a stored conversation
// When it is resumed after the settings changed
// Then it follows the settings, as `permissions.automatic_review` does
#[tokio::test]
async fn a_resume_reads_the_current_setting() {
    for before in [true, false] {
        let env = Env::new();
        let state = state(&env);
        settings(&env, Some(before), plain_agents());
        let id = state.create_session(&env.ws, None, false).await.unwrap();
        assert_eq!(yolo_of(&state, id).await, before);
        state.close_session(id).await.unwrap();
        state.history().flush().await.unwrap();
        settings(&env, Some(!before), plain_agents());
        let (resumed, was_resumed) = state
            .create_or_resume_session(&env.ws, None, false, Some(id))
            .await
            .unwrap();
        assert!(was_resumed);
        assert_eq!(resumed, id);
        assert_eq!(yolo_of(&state, id).await, !before);
    }
}

// Given a request that was pending when the process ended
// When the conversation is resumed with yolo on
// Then the request expires and its call never runs: yolo revives nothing
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_in_yolo_does_not_revive_a_pending_permission() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let write = http_sse(&sse_body(&[&tool_calls_chunk(&[(
        "w1",
        "write",
        json!({"path": target, "content": "from w1"}),
    )])]));
    let done = http_sse(&sse_body(&[&text_chunk("after restart")]));
    let provider = spawn_capturing_provider(vec![write, done]).await;
    let config = || TestServerConfig::mock(&env.root, &provider.url).rules_home(&env.home);

    let first = config().state();
    let id = first.create_session(&env.ws, None, false).await.unwrap();
    assert!(!yolo_of(&first, id).await);
    first
        .send(captain_command(id, "write it".to_string()))
        .await
        .unwrap();
    let requested = wait_for_kind(&first, id, "permission.requested").await;
    wait_for_kind(&first, id, "turn.end").await;
    await_transcript(&first, id).await;

    // The process "dies"; the next one starts with yolo on.
    let second = config().state();
    let mut document: Value =
        serde_json::from_str(&std::fs::read_to_string(env.root.join("cfg/config.json")).unwrap())
            .unwrap();
    document["yolo"] = json!(true);
    std::fs::write(env.root.join("cfg/config.json"), document.to_string()).unwrap();
    let (resumed, _) = second
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    assert_eq!(resumed, id);
    assert!(yolo_of(&second, id).await);

    let expired = wait_for_kind(&second, id, "permission.resolved").await;
    assert_eq!(expired["payload"]["resolution"], "expired");
    assert_eq!(
        expired["payload"]["request_id"],
        requested["payload"]["request_id"]
    );
    let (status, _) = request_json(
        &app(second.clone()),
        "POST",
        &format!(
            "/v1/sessions/{id}/permissions/{}",
            requested["payload"]["request_id"].as_str().unwrap()
        ),
        Some(&json!({"decision": "allow_once"})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert!(!target.exists(), "the lost call is not replayed");
    assert!(
        events(&second, id, "tool.post_call")
            .await
            .iter()
            .all(|e| e.payload["metadata"]["approved_by"] != "yolo")
    );
}

async fn wait_for_kind(state: &AppState, id: ulid::Ulid, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(found) = events(state, id, kind).await.into_iter().next() {
                return serde_json::to_value(found).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for `{kind}`"))
}

async fn await_transcript(state: &AppState, id: ulid::Ulid) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            state.history().flush().await.expect("flush");
            let stored = state.history().load(id).await.unwrap().expect("session");
            if !stored.transcript.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the transcript is stored");
}

// Given a permission database that cannot be opened
// Then normal sessions start with a visible warning, and yolo remains unchecked.
#[tokio::test]
async fn an_unavailable_permission_database_starts_with_a_warning_and_yolo_is_unchanged() {
    let env = Env::new();
    let state = state(&env);
    settings(&env, None, plain_agents());
    // A directory where the database file should be: it cannot be opened.
    std::fs::create_dir_all(env.root.join("data/withnav.db")).unwrap();
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let summary = state.session_summary(id).await.unwrap();
    assert!(
        summary
            .permissions_unavailable
            .as_deref()
            .unwrap()
            .contains("withnav.db")
    );
    state.close_session(id).await.unwrap();
    settings(&env, Some(true), plain_agents());
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    assert!(yolo_of(&state, id).await);
    state.close_session(id).await.unwrap();
}
