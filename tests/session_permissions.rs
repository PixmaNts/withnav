//! The permission gate through the session service and the HTTP surface: a
//! gated tool call yields the turn, the Captain answers with a typed decision,
//! and the decision (never chat text) resumes it.

use std::path::Path;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{json, Value};

use withnav::bus::command::{Command, ContentBlock, DeliveryMode};
use withnav::bus::event::EventEnvelope;
use withnav::server::{app, AppState};

mod common;
use common::server::{captain_command, request_json, stored_events as events, TestServerConfig};
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_streaming_provider, sse_body, text_chunk,
    text_response as done, tool_call_chunk, tool_calls_chunk, CapturingProvider,
};

use common::TestEnv as Env;

fn config(env: &Env, provider: &CapturingProvider) -> TestServerConfig {
    TestServerConfig::mock(&env.root, &provider.url).rules_home(&env.root.join("home"))
}

/// One assistant message calling each `(id, tool, arguments)`.
fn calls(calls: &[(&str, &str, Value)]) -> Vec<u8> {
    http_sse(&sse_body(&[&tool_calls_chunk(calls)]))
}

fn write_call(id: &str, path: &Path) -> (String, Value) {
    (
        id.to_string(),
        json!({"path": path, "content": format!("from {id}")}),
    )
}

fn one_write(id: &str, path: &Path) -> Vec<u8> {
    let (id, args) = write_call(id, path);
    calls(&[(&id, "write", args)])
}

async fn provider(responses: Vec<Vec<u8>>) -> CapturingProvider {
    spawn_capturing_provider(responses).await
}

async fn say(state: &AppState, id: ulid::Ulid, text: &str) {
    state
        .send(captain_command(id, text.to_string()))
        .await
        .expect("send");
}

/// Posts `text` into the session as the navigator would (an in-process wake).
async fn wake(state: &AppState, id: ulid::Ulid, text: &str) {
    state
        .send(Command {
            command_id: ulid::Ulid::generate(),
            session_id: id,
            source_actor_id: withnav::navigator::NAVIGATOR_ACTOR_ID.to_string(),
            target_actor_id: "agent:main".to_string(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            created_at: std::time::SystemTime::now(),
        })
        .await
        .expect("wake");
}

/// Waits until at least `count` events of `kind` exist; returns them all.
async fn wait_events(
    state: &AppState,
    id: ulid::Ulid,
    kind: &str,
    count: usize,
) -> Vec<EventEnvelope> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let sub = state.subscribe(id, None).await.expect("subscribe");
        let mut found = match sub.replay {
            withnav::bus::ResumeOutcome::Replay(all) => all
                .into_iter()
                .filter(|e| e.kind == kind)
                .collect::<Vec<_>>(),
            other => panic!("session {id}: expected replay for `{kind}`, got {other:?}"),
        };
        let mut live = sub.live;
        while found.len() < count {
            let event = live
                .as_mut()
                .unwrap_or_else(|| panic!("session {id}: ended before {count} `{kind}` events"))
                .recv()
                .await
                .unwrap_or_else(|| panic!("session {id}: stream closed waiting for `{kind}`"));
            if event.kind == kind {
                found.push(event);
            }
        }
        found
    })
    .await
    .unwrap_or_else(|_| panic!("session {id}: timed out waiting for {count} `{kind}` events"))
}

async fn answer(
    router: &axum::Router,
    id: ulid::Ulid,
    request: &str,
    body: Value,
) -> (StatusCode, Value) {
    request_json(
        router,
        "POST",
        &format!("/v1/sessions/{id}/permissions/{request}"),
        Some(&body),
    )
    .await
}

async fn await_persisted_transcript(state: &AppState, id: ulid::Ulid) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            state
                .history()
                .flush()
                .await
                .expect("flush transcript writer");
            let stored = state
                .history()
                .load(id)
                .await
                .expect("load transcript")
                .expect("session");
            if !stored.transcript.is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("session {id}: yielded transcript was not persisted"));
}

fn request_id(event: &EventEnvelope) -> String {
    event.payload["request_id"].as_str().unwrap().to_string()
}

fn body_of(provider: &CapturingProvider, n: usize) -> String {
    provider.bodies()[n].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_unknown_tool_asks_always_saves_global_rule_and_yolo_runs_directly() {
    for yolo in [false, true] {
        let env = Env::new();
        let tool = "mcp__fake__echo";
        let provider = provider(vec![
            calls(&[("echo-first", tool, json!({"text":"first"}))]),
            done("first finished"),
            calls(&[("echo-next", tool, json!({"text":"next"}))]),
            done("next finished"),
        ])
        .await;
        let cfg = config(&env, &provider).config();
        let mut settings = withnav::settings::load(&cfg.config_dir).unwrap();
        settings.set_dotted("mcp", json!({"fake":{"command":["python3",concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/mcp_server.py")]}}));
        settings.set_dotted("yolo", json!(yolo));
        withnav::settings::save(&cfg.config_dir, &settings).unwrap();
        // The mock has no instinct assignment: the built-in permission flow applies the
        // rule on unavailable judgement, asking with no rule and allowing the saved rule.
        let state = AppState::new(cfg).unwrap();
        common::server::wait_for("MCP discovery", || {
            state
                .service()
                .mcp_statuses()
                .iter()
                .any(|s| s.server_id == "fake" && s.state == "ready")
        })
        .await;
        let router = app(state.clone());
        let id = state.create_session(&env.ws, None, false).await.unwrap();
        say(&state, id, "echo first").await;
        if !yolo {
            let requested = wait_events(&state, id, "permission.requested", 1).await;
            assert_eq!(requested[0].payload["tool"], tool);
            assert_eq!(
                requested[0].payload["always_label"],
                format!("Always (global): {tool} *")
            );
            assert!(events(&state, id, "tool.post_call").await.is_empty());
            let (status, body) = answer(
                &router,
                id,
                &request_id(&requested[0]),
                json!({"decision":"allow_always"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        wait_events(&state, id, "task.done", 1).await;
        say(&state, id, "echo next").await;
        wait_events(&state, id, "task.done", 2).await;
        let posts = events(&state, id, "tool.post_call").await;
        assert_eq!(posts.len(), 2);
        assert_eq!(
            posts[0].payload["metadata"]["approved_by"],
            if yolo { "yolo" } else { "captain" }
        );
        assert_eq!(
            posts[1].payload["metadata"]["approved_by"],
            if yolo { "yolo" } else { "rule" }
        );
        assert!(provider.bodies()[1].contains("first"));
        assert!(provider.bodies()[3].contains("next"));
        assert_eq!(
            events(&state, id, "permission.requested").await.len(),
            usize::from(!yolo)
        );
        let rules = withnav::permission::store::PermissionStore::open(env.root.join("data"))
            .unwrap()
            .list(&Default::default())
            .unwrap();
        if yolo {
            assert!(rules.is_empty());
        } else {
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].action, tool);
            assert_eq!(rules[0].resource, "*");
            assert_eq!(rules[0].effect, withnav::permission::rules::Effect::Allow);
            assert_eq!(
                rules[0].scope,
                withnav::permission::rules::RuleScope::Global
            );
        }
        state.service().close_all().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_reject_does_not_claim_the_pending_request() {
    for exact in ["a".repeat(2048), "é".repeat(1024)] {
        let env = Env::new();
        let target = env.ws.join("rejected.txt");
        let provider = provider(vec![one_write("w1", &target), done("finished")]).await;
        let state = config(&env, &provider).state();
        let router = app(state.clone());
        let id = state.create_session(&env.ws, None, false).await.unwrap();
        say(&state, id, "write rejected.txt").await;
        let requested = wait_events(&state, id, "permission.requested", 1).await;
        let request = request_id(&requested[0]);
        for message in [format!("{exact}a"), format!("{}é", "a".repeat(2047))] {
            let (status, body) = answer(
                &router,
                id,
                &request,
                json!({"decision":"reject","message":message}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body["error"].as_str().unwrap().contains("2048 UTF-8 bytes"));
            let error = state
                .answer_permission(
                    id,
                    request.parse().unwrap(),
                    withnav::agent::permissions::PermissionDecision::Reject {
                        message: Some(message),
                    },
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("2048 UTF-8 bytes"));
            assert!(events(&state, id, "permission.resolved").await.is_empty());
        }
        let (status, body) = answer(
            &router,
            id,
            &request,
            json!({"decision":"reject","message":exact}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        wait_events(&state, id, "task.done", 1).await;
        assert!(!target.exists());
        state.close_session(id).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_fetch_saves_the_origin_and_releases_the_second_call() {
    let env = Env::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let url = format!("{origin}/page");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route("/page", axum::routing::get(|| async { "page" })),
        )
        .await
        .unwrap();
    });
    let provider = provider(vec![
        calls(&[
            ("f1", "fetch", json!({"url":url})),
            ("f2", "fetch", json!({"url":url})),
        ]),
        done("finished"),
    ])
    .await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    say(&state, id, "fetch the page twice").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    assert_eq!(
        requested[0].payload["always_label"],
        format!("Always (project): fetch {origin}/*")
    );
    let (status, body) = answer(
        &router,
        id,
        &request_id(&requested[0]),
        json!({"decision":"allow_always"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_events(&state, id, "task.done", 1).await;
    // The saved rule let the second call of the turn run without a question.
    assert_eq!(events(&state, id, "permission.requested").await.len(), 1);
    let posts = events(&state, id, "tool.post_call").await;
    let approved: Vec<_> = posts
        .iter()
        .map(|p| p.payload["metadata"]["approved_by"].clone())
        .collect();
    assert_eq!(approved, [json!("captain"), json!("rule")]);
    let rules = withnav::permission::store::PermissionStore::open(env.root.join("data"))
        .unwrap()
        .list(&withnav::permission::rules::RuleFilter::default())
        .unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].resource, format!("{origin}/*"));
    state.close_session(id).await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_write_asks_the_captain_and_each_answer_runs_once() {
    for decision in ["allow_once", "allow_config_session"] {
        let env = Env::new();
        let target = env.root.join("cfg/config.json");
        let provider = provider(vec![
            calls(&[
                ("w1", "write", json!({"path":target,"content":"first"})),
                ("w2", "write", json!({"path":target,"content":"second"})),
            ]),
            done("finished"),
        ])
        .await;
        let state = config(&env, &provider).state();
        let original = std::fs::read_to_string(&target).unwrap();
        let router = app(state.clone());
        let id = state.create_session(&env.ws, None, false).await.unwrap();
        say(&state, id, "write the configuration").await;
        let requested = wait_events(&state, id, "permission.requested", 1).await;
        // No instinct answers the protected check here: only the Captain can.
        assert_eq!(requested[0].payload["requires_captain"], true);
        assert!(requested[0].payload.get("always_label").is_none());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), original);
        // "Always" is never offered for it.
        let (status, _) = answer(
            &router,
            id,
            &request_id(&requested[0]),
            json!({"decision":"allow_always"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, body) = answer(
            &router,
            id,
            &request_id(&requested[0]),
            json!({"decision":decision}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The legacy session-wide answer is one approval too.
        let requested = wait_events(&state, id, "permission.requested", 2).await;
        assert_eq!(requested[1].payload["tool_call_id"], "w2");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");
        let (status, body) = answer(
            &router,
            id,
            &request_id(&requested[1]),
            json!({"decision":"reject"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        wait_events(&state, id, "task.done", 1).await;
        state.close_session(id).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_answers_and_legacy_answers_apply_to_the_held_call() {
    for decision in [
        "allow_once",
        "allow_always",
        "reject",
        "allow_folder",
        "deny",
    ] {
        let env = Env::new();
        let target = env.ws.join("answer.txt");
        let provider = provider(vec![one_write("w1", &target), done("finished")]).await;
        let state = config(&env, &provider).state();
        let router = app(state.clone());
        let id = state.create_session(&env.ws, None, false).await.unwrap();
        say(&state, id, "write answer.txt").await;
        let requested = wait_events(&state, id, "permission.requested", 1).await;
        assert_eq!(
            requested[0].payload["always_label"],
            format!("Always (project): write {}", target.display())
        );
        let (status, body) = answer(
            &router,
            id,
            &request_id(&requested[0]),
            json!({"decision":decision,"message":"Choose another approach"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        wait_events(&state, id, "task.done", 1).await;
        let rejected = matches!(decision, "reject" | "deny");
        assert_eq!(target.exists(), !rejected);
        if rejected {
            assert!(body_of(&provider, 1).contains("Choose another approach"));
        }
        let resolved = events(&state, id, "permission.resolved").await;
        assert_eq!(
            resolved[0].payload["resolution"],
            if decision == "allow_folder" {
                "allow_once"
            } else {
                decision
            }
        );
        state.close_session(id).await.unwrap();
    }
}

// Given a write, when the Captain allows it once
// Then the call runs once, its real result is what the next model request
//      carries, and the request is stored with its resolution
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_once_runs_the_call_and_the_model_sees_its_result() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target), done("written")]).await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write a.txt").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    let payload = &requested[0].payload;
    assert_eq!(payload["tool"], "write");
    assert_eq!(payload["tool_call_id"], "w1");
    assert_eq!(payload["agent"], "build");
    assert_eq!(payload["scope"], json!({"kind": "folder", "value": env.ws}));
    assert!(payload["arguments"].as_str().unwrap().len() <= 4096);
    assert!(!target.exists(), "the call has not run");
    wait_events(&state, id, "turn.end", 1).await;

    // Commands that change the session are refused while a request waits.
    let (_, refused) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/commands"),
        Some(&json!({"name": "agent", "arguments": "plan"})),
    )
    .await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("approval"));

    let request = request_id(&requested[0]);
    let (status, body) = answer(&router, id, &request, json!({"decision": "allow_once"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], true);

    wait_events(&state, id, "task.done", 1).await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "from w1");
    let next = body_of(&provider, 1);
    assert!(next.contains("\"tool_call_id\":\"w1\""), "{next}");
    assert!(next.contains("wrote "), "the real result: {next}");

    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["request_id"], request.as_str());
    assert_eq!(resolved[0].payload["resolution"], "allow_once");
    // Stored: a stopped conversation replays both events.
    state.close_session(id).await.unwrap();
    assert_eq!(events(&state, id, "permission.requested").await.len(), 1);
    assert_eq!(events(&state, id, "permission.resolved").await.len(), 1);

    // A duplicate and a request that never existed are stale; an unknown
    // session is 404.
    let (stale, body) = answer(&router, id, &request, json!({"decision": "allow_once"})).await;
    assert_ne!(stale, StatusCode::OK, "{body}");
    let live = {
        let provider = self::provider(vec![one_write("w9", &target), done("x")]).await;
        let state = config(&env, &provider).state();
        let id = state.create_session(&env.ws, None, false).await.unwrap();
        say(&state, id, "again").await;
        let requested = wait_events(&state, id, "permission.requested", 1).await;
        (app(state), id, request_id(&requested[0]))
    };
    let (router2, id2, request2) = live;
    let (status, body) = answer(&router2, id2, &request2, json!({"decision": "allow_once"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = answer(&router2, id2, &request2, json!({"decision": "allow_once"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate: {body}");
    assert_eq!(body["reason"], "permission_stale");
    let (status, _) = answer(
        &router2,
        id2,
        &ulid::Ulid::generate().to_string(),
        json!({"decision": "allow_once"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "unknown request id");
    let (status, _) = answer(
        &router2,
        ulid::Ulid::generate(),
        &request2,
        json!({"decision": "allow_once"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown session");
    let (status, _) = answer(&router2, id2, &request2, json!({"decision": "maybe"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "not a decision");
}

fn authenticated_router(state: withnav::server::AppState) -> axum::Router {
    app(state).layer(axum::middleware::from_fn(
        |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
            request
                .headers_mut()
                .insert("authorization", "Bearer test-token".parse().unwrap());
            next.run(request).await
        },
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bypass_resolves_pending_permission_once_with_bypass_provenance() {
    let env = Env::new();
    let target = env.ws.join("bypass.txt");
    let provider = provider(vec![one_write("bypass-write", &target), done("written")]).await;
    let state = config(&env, &provider).bearer_token("test-token").state();
    let router = authenticated_router(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    say(&state, id, "write the file").await;
    wait_events(&state, id, "permission.requested", 1).await;
    for _ in 0..2 {
        let (status, body) = request_json(
            &router,
            "POST",
            &format!("/v1/sessions/{id}/bypass"),
            Some(&json!({"enabled":true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    wait_events(&state, id, "task.done", 1).await;
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "from bypass-write"
    );
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["approved_by"], "bypass");
    let posts = events(&state, id, "tool.post_call").await;
    assert_eq!(posts.len(), 1);
    assert!(serde_json::to_string(&posts[0].payload)
        .unwrap()
        .contains("bypass"));
    state.close_session(id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_runs_a_protected_write_without_a_request_and_says_so() {
    let env = Env::new();
    let target = env.ws.join("auth.json");
    let provider = provider(vec![one_write("yolo-write", &target), done("written")]).await;
    let state = config(&env, &provider).state();
    let settings = env.root.join("cfg/config.json");
    let mut document: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    document["yolo"] = json!(true);
    std::fs::write(&settings, document.to_string()).unwrap();
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    assert!(state.session_summary(id).await.unwrap().yolo);
    say(&state, id, "write credentials").await;
    wait_events(&state, id, "task.done", 1).await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "from yolo-write");
    assert!(events(&state, id, "permission.requested").await.is_empty());
    let posts = events(&state, id, "tool.post_call").await;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].payload["metadata"]["approved_by"], "yolo");
    state.close_session(id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bypass_leaves_pending_sensitive_request_open() {
    let env = Env::new();
    let target = env.ws.join("auth.json");
    let provider = provider(vec![one_write("protected-write", &target)]).await;
    let state = config(&env, &provider).bearer_token("test-token").state();
    let router = authenticated_router(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    say(&state, id, "write credentials").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    assert_eq!(requested[0].payload["requires_captain"], true);
    let (status, body) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/bypass"),
        Some(&json!({"enabled":true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(events(&state, id, "permission.resolved").await.is_empty());
    assert!(!target.exists());
    state.close_session(id).await.unwrap();
    assert!(!target.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_captain_answer_keeps_captain_provenance_after_bypass() {
    let env = Env::new();
    let target = env.ws.join("captain.txt");
    let provider = provider(vec![one_write("captain-write", &target), done("written")]).await;
    let state = config(&env, &provider).bearer_token("test-token").state();
    let router = authenticated_router(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    say(&state, id, "write the file").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    let (status, body) = answer(
        &router,
        id,
        &request_id(&requested[0]),
        json!({"decision":"allow_once"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/bypass"),
        Some(&json!({"enabled":true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_events(&state, id, "task.done", 1).await;
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["approved_by"], "captain");
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "from captain-write"
    );
    state.close_session(id).await.unwrap();
}

// Given allow-folder on one folder
// Then a write below it runs without a request, a write in another folder
//      asks, and a denial with a message reaches the model as
//      `permission_denied`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_folder_covers_descendants_and_deny_reaches_the_model() {
    let env = Env::new();
    let (first, nested, other) = (
        env.ws.join("src/a.txt"),
        env.ws.join("src/deep/b.txt"),
        env.ws.join("other/c.txt"),
    );
    let provider = provider(vec![
        one_write("w1", &first),
        one_write("w2", &nested),
        one_write("w3", &other),
        done("understood"),
    ])
    .await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write three files").await;
    let first_request = wait_events(&state, id, "permission.requested", 1).await;
    let (status, _) = answer(
        &router,
        id,
        &request_id(&first_request[0]),
        json!({"decision": "allow_folder"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The old folder answer is one approval; even the descendant asks again.
    let requested = wait_events(&state, id, "permission.requested", 2).await;
    assert_eq!(requested.len(), 2, "exactly one more request");
    assert_eq!(requested[1].payload["tool_call_id"], "w2");
    assert!(first.exists() && !nested.exists(), "only w1 ran");
    assert!(!other.exists());

    answer(
        &router,
        id,
        &request_id(&requested[1]),
        json!({"decision": "allow_once"}),
    )
    .await;
    let requested = wait_events(&state, id, "permission.requested", 3).await;
    assert_eq!(requested[2].payload["tool_call_id"], "w3");
    assert!(nested.exists());

    let (status, _) = answer(
        &router,
        id,
        &request_id(&requested[2]),
        json!({"decision": "deny", "message": "keep out of other/"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_events(&state, id, "task.done", 1).await;
    assert!(!other.exists(), "a denied call never runs");
    let last = body_of(&provider, 3);
    assert!(last.contains("permission_denied"), "{last}");
    assert!(last.contains("keep out of other/"), "{last}");

    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved[0].payload["resolution"], "allow_once");
    assert_eq!(resolved[2].payload["resolution"], "deny");
    assert_eq!(resolved[2].payload["message"], "keep out of other/");
}

// Given two writes in one assistant message
// When the first is denied
// Then the second is still processed (it asks on its own)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reject_settles_every_held_sibling_with_the_captains_message() {
    let env = Env::new();
    let (a, b) = (env.ws.join("a.txt"), env.ws.join("b.txt"));
    let (id_a, args_a) = write_call("w1", &a);
    let (id_b, args_b) = write_call("w2", &b);
    let provider = provider(vec![
        calls(&[(&id_a, "write", args_a), (&id_b, "write", args_b)]),
        done("ok"),
    ])
    .await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "go").await;
    let first = wait_events(&state, id, "permission.requested", 1).await;
    answer(
        &router,
        id,
        &request_id(&first[0]),
        json!({"decision": "reject", "message": "use the template instead"}),
    )
    .await;
    wait_events(&state, id, "task.done", 1).await;
    assert_eq!(events(&state, id, "permission.requested").await.len(), 1);
    assert!(!a.exists() && !b.exists());
    let last = body_of(&provider, 1);
    assert!(last.contains("\"tool_call_id\":\"w1\"") && last.contains("\"tool_call_id\":\"w2\""));
    assert_eq!(
        last.matches("use the template instead").count(),
        2,
        "{last}"
    );
}

// Given a pending request
// When chat text arrives
// Then it never approves: the request is cancelled, nothing runs, and the
//      text is an ordinary message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_text_never_approves() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target), done("fine")]).await;
    let state = config(&env, &provider).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write it").await;
    wait_events(&state, id, "permission.requested", 1).await;
    say(&state, id, "yes, allow it").await;
    let resolved = wait_events(&state, id, "permission.resolved", 1).await;
    assert_eq!(resolved[0].payload["resolution"], "cancelled");
    wait_events(&state, id, "task.done", 1).await;
    assert!(!target.exists());
    let next = body_of(&provider, 1);
    assert!(next.contains("yes, allow it"));
}

// Given a pending request
// When the session is stopped
// Then the held call never runs and permission.resolved says cancelled
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_while_a_request_is_pending_cancels_it() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target)]).await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write it").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    wait_events(&state, id, "turn.end", 1).await;
    state.close_session(id).await.unwrap();

    assert!(!target.exists());
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["resolution"], "cancelled");
    assert_eq!(
        resolved[0].payload["request_id"],
        requested[0].payload["request_id"]
    );
    let (status, _) = answer(
        &router,
        id,
        &request_id(&requested[0]),
        json!({"decision": "allow_once"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a stopped session needs a resume"
    );
}

// Given a request that was pending when the process ended
// When the conversation is resumed
// Then the request expires, its call is settled as interrupted and never
//      replayed, and the next model request carries the settled result
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_expires_the_pending_request_without_replaying_it() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target), done("after restart")]).await;
    let first = config(&env, &provider).state();
    let id = first.create_session(&env.ws, None, false).await.unwrap();
    say(&first, id, "write it").await;
    let requested = wait_events(&first, id, "permission.requested", 1).await;
    wait_events(&first, id, "turn.end", 1).await;
    // The transcript snapshot of the yielded turn is stored.
    await_persisted_transcript(&first, id).await;
    // The process "dies": no stop, no resolution.
    let second = config(&env, &provider).state();
    let (resumed, _) = second
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    assert_eq!(resumed, id);

    let expired = wait_events(&second, id, "permission.resolved", 1).await;
    assert_eq!(expired[0].payload["resolution"], "expired");
    assert_eq!(
        expired[0].payload["request_id"],
        requested[0].payload["request_id"]
    );
    let router = app(second.clone());
    let (status, _) = answer(
        &router,
        id,
        &request_id(&requested[0]),
        json!({"decision": "allow_once"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "an expired request is stale");

    // The decision table says the question expired.
    let decisions = withnav::permission::store::PermissionStore::open(env.root.join("data"))
        .unwrap()
        .list_decisions(
            &withnav::permission::decisions::DecisionFilter {
                session_id: Some(id.to_string()),
                ..Default::default()
            },
            10,
        )
        .unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        decisions[0].outcome,
        withnav::permission::decisions::Outcome::Expired
    );

    say(&second, id, "carry on").await;
    wait_events(&second, id, "task.done", 1).await;
    assert!(!target.exists(), "nothing is replayed");
    let next = body_of(&provider, 1);
    assert!(next.contains("\"tool_call_id\":\"w1\""), "{next}");
    assert!(next.contains("carry on"));
}

fn run_withnav(root: &Path, ws: &Path, models: &Path, extra: &[&str]) -> std::process::Output {
    std::fs::create_dir_all(root.join("config/withnav")).unwrap();
    std::fs::write(
        root.join("config/withnav/config.json"),
        r#"{"agents":{"build":{"model":"mock/mock-model"}}}"#,
    )
    .unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(&mut command, root);
    command
        .args(["run", "--workspace"])
        .arg(ws)
        .arg("--models")
        .arg(models)
        .args(extra)
        .env("WITHNAV_TEST_KEY", "test-key")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .output()
        .expect("run the binary")
}

// Given `nav run` reaching a gated call
// Then it stops with `permission_required` and exit 3, never hanging or
//      approving; with --yes the session's bypass lets ordinary work finish
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withnav_run_stops_on_a_permission_request_unless_yes() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![
        one_write("w1", &target),
        one_write("w2", &target),
        done("done"),
    ])
    .await;
    let cfg = config(&env, &provider);
    cfg.config();
    let models = cfg.models_path();

    let (root_env, models_a) = (env.root.clone(), models.clone());
    let ws = env.ws.clone();
    let stopped = tokio::task::spawn_blocking(move || {
        run_withnav(&root_env, &ws, &models_a, &["--prompt", "write a.txt"])
    })
    .await
    .unwrap();
    assert_eq!(stopped.status.code(), Some(3), "{stopped:?}");
    let stderr = String::from_utf8_lossy(&stopped.stderr);
    assert!(
        stderr.contains(&format!("permission_required: write {}", env.ws.display())),
        "{stderr}"
    );
    assert!(!target.exists());

    let (root_env, ws) = (env.root.clone(), env.ws.clone());
    let approved = tokio::task::spawn_blocking(move || {
        run_withnav(
            &root_env,
            &ws,
            &models,
            &["--yes", "--prompt", "write a.txt"],
        )
    })
    .await
    .unwrap();
    assert_eq!(approved.status.code(), Some(0), "{approved:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "from w2");
}

// Given `nav run --yes` whose model writes a credentials file
// Then the hard floor holds: it prints the request with its reason, exits 3
//      and writes nothing
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withnav_run_yes_stops_at_the_hard_floor_without_side_effects() {
    let env = Env::new();
    let target = env.ws.join("auth.json");
    let provider = provider(vec![one_write("w1", &target), done("never")]).await;
    let cfg = config(&env, &provider);
    cfg.config();
    let models = cfg.models_path();
    let (root_env, ws) = (env.root.clone(), env.ws.clone());
    let output = tokio::task::spawn_blocking(move || {
        run_withnav(
            &root_env,
            &ws,
            &models,
            &["--yes", "--prompt", "write auth.json"],
        )
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("permission_required: write "), "{stderr}");
    assert!(
        stderr.contains(&format!("{}: ", env.ws.display())),
        "{stderr}"
    );
    assert!(!target.exists());
    assert_eq!(
        provider.requests().len(),
        1,
        "the run stopped at the request"
    );
}

// Given `nav run --yes` where a child session reaches the hard floor
// Then the mirrored request ends the whole run with exit 3, and the child
//      writes nothing
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withnav_run_yes_stops_when_a_child_reaches_the_hard_floor() {
    let env = Env::new();
    let agents = env.ws.join(".withnav/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite the requested change.",
    )
    .unwrap();
    let target = env.ws.join("auth.json");
    let child_target = target.clone();
    // The parent's second answer is held open, so the child's request is what
    // ends the run.
    let mut provider = spawn_streaming_provider().await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url).rules_home(&env.root.join("home"));
    // On 2026-10-12, reqwest's platform verifier changed client startup timing:
    // the parent's next request can precede the child's first request. Hold
    // by conversation identity, not arrival index, and keep accepting children.
    let responder = tokio::spawn(async move {
        let mut spawned = false;
        let mut held = Vec::new();
        while let Some((request, response)) = provider.requests.recv().await {
            if request.contains("Write the requested change.") {
                response
                    .send(tool_call_chunk(
                        "child-write",
                        "write",
                        &json!({
                            "path": child_target, "content": "secret"
                        }),
                    ))
                    .unwrap();
            } else if !spawned {
                spawned = true;
                response
                    .send(tool_call_chunk(
                        "spawn",
                        "task",
                        &json!({
                            "agent": "writer", "prompt": "write auth.json"
                        }),
                    ))
                    .unwrap();
            } else {
                held.push(response);
            }
        }
    });
    cfg.config();
    let models = cfg.models_path();
    let started = std::time::Instant::now();
    let (root_env, ws) = (env.root.clone(), env.ws.clone());
    let output = tokio::task::spawn_blocking(move || {
        run_withnav(&root_env, &ws, &models, &["--yes", "--prompt", "delegate"])
    })
    .await
    .unwrap();
    responder.abort();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "ended promptly");
    assert!(!target.exists());
}

// Given a decision already taken in while the resumed turn stalls
// When a second decision arrives for the same request
// Then it is refused at once, not after the turn
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_answer_is_refused_at_once_while_the_resumed_turn_runs() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let mut provider = spawn_streaming_provider().await;
    let state = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.root.join("home"))
        .state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write it").await;
    let (_, first) = provider.requests.recv().await.expect("first request");
    first
        .send(tool_call_chunk(
            "w1",
            "write",
            &json!({"path": target, "content": "x"}),
        ))
        .unwrap();
    drop(first);
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    let request = request_id(&requested[0]);

    let (status, body) = answer(&router, id, &request, json!({"decision": "allow_once"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The continuation asks the model and stalls there.
    let (_, stalled) = provider.requests.recv().await.expect("continuation");

    let second = tokio::time::timeout(
        Duration::from_secs(2),
        answer(&router, id, &request, json!({"decision": "deny"})),
    )
    .await
    .expect("the duplicate must not wait for the resumed turn");
    assert_eq!(second.0, StatusCode::CONFLICT, "{}", second.1);
    assert_eq!(second.1["reason"], "permission_stale");

    stalled.send(text_chunk("done")).unwrap();
    drop(stalled);
    wait_events(&state, id, "task.done", 1).await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
}

// Given a pending request
// When `/stop` is sent as a command
// Then it is accepted, the request ends as cancelled and nothing runs
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stop_command_is_accepted_while_a_request_is_pending() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target)]).await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write it").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    wait_events(&state, id, "turn.end", 1).await;

    let (_, stop) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/commands"),
        Some(&json!({"name": "stop", "arguments": ""})),
    )
    .await;
    assert_eq!(stop["ok"], true, "{stop}");

    assert!(!target.exists());
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["resolution"], "cancelled");
    assert_eq!(
        resolved[0].payload["request_id"],
        requested[0].payload["request_id"]
    );
}

// Given a permission request that waits for a decision
// When /compact runs, now or accepted behind the turn that raised the request
// Then it is refused, nothing is summarized, and a deferred one is refused
//      under the operation id it was accepted with
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_while_a_permission_request_is_open_is_refused() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target), done("written")]).await;
    let state = config(&env, &provider).chat_tools(20_000).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write a.txt").await;
    wait_events(&state, id, "permission.requested", 1).await;
    wait_events(&state, id, "turn.end", 1).await;

    let out = state.run_command(id, "compact", "").await.unwrap();
    assert!(!out.ok, "{out:?}");
    let error = out.error.unwrap();
    assert!(
        error.contains("permission request"),
        "the reason names the pending request: {error}"
    );
    assert_eq!(provider.bodies().len(), 1, "no summary request");
    assert!(events(&state, id, "context.compacted").await.is_empty());
    assert!(!target.exists(), "the held call did not run");
}

// Given a /compact accepted while a turn runs
// When the turn ends on a permission request
// Then the deferred run is refused under its own operation id
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deferred_compact_is_refused_when_the_turn_ends_on_a_permission_request() {
    let env = Env::new();
    let target = env.ws.join("a.txt");
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = common::sse::spawn_gated_provider(
        vec![one_write("w1", &target), done("written")],
        release.clone(),
    )
    .await;
    let state = config(&env, &provider).chat_tools(20_000).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write a.txt").await;
    common::server::wait_for("the turn's request", || provider.bodies().len() == 1).await;
    let accepted = state.run_command(id, "compact", "").await.unwrap();
    assert_eq!(accepted.data.as_ref().unwrap()["accepted"], true);
    release.notify_one();
    wait_events(&state, id, "permission.requested", 1).await;

    let refusal = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let results = events(&state, id, "command.result").await;
            if let Some(refusal) = results
                .into_iter()
                .find(|e| e.payload["name"] == "compact" && e.payload["ok"] == false)
            {
                return refusal;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a refusal");
    assert_eq!(
        refusal.payload["operation_id"],
        accepted.operation_id.to_string()
    );
    assert!(refusal.payload["error"]
        .as_str()
        .unwrap()
        .contains("permission request"));
    assert_eq!(provider.bodies().len(), 1, "no summary request");
    assert!(events(&state, id, "context.compacted").await.is_empty());
}

/// Waits for request number `n` (1-based) and returns its id.
async fn nth_request(state: &AppState, id: ulid::Ulid, n: usize) -> ulid::Ulid {
    let found = wait_events(state, id, "permission.requested", n).await;
    request_id(&found[n - 1]).parse().unwrap()
}

async fn nth_suspended_request(state: &AppState, id: ulid::Ulid, n: usize) -> ulid::Ulid {
    let request = nth_request(state, id, n).await;
    let ended = wait_events(state, id, "turn.end", n).await;
    assert_eq!(ended[n - 1].payload["outcome"], "yielded");
    request
}

async fn allow(state: &AppState, id: ulid::Ulid, request: ulid::Ulid) {
    state
        .answer_permission(
            id,
            request,
            withnav::agent::permissions::PermissionDecision::AllowOnce,
        )
        .await
        .expect("answer");
}

// Given a model message with two writes, and a navigator wake posted while
//       the first request waits
// When the Captain approves the first, the second raises its own request, and
//      the Captain approves that too
// Then no request is cancelled, both files are written, and the wake runs
//      as its own turn only after the turn that held the requests has ended
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_held_behind_a_request_does_not_cancel_the_next_request() {
    let env = Env::new();
    let (a, b) = (env.ws.join("a.txt"), env.ws.join("b.txt"));
    let (id_a, args_a) = write_call("w1", &a);
    let (id_b, args_b) = write_call("w2", &b);
    let provider = provider(vec![
        calls(&[(&id_a, "write", args_a), (&id_b, "write", args_b)]),
        done("both written"),
        done("wake seen"),
    ])
    .await;
    let state = config(&env, &provider).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write both").await;
    let first = nth_request(&state, id, 1).await;
    wake(&state, id, "drift: look at the parser").await;
    // The wake is accepted (durable) while the request waits...
    wait_events(&state, id, "message.received", 2).await;
    assert!(events(&state, id, "permission.resolved").await.is_empty());

    allow(&state, id, first).await;
    let second = nth_suspended_request(&state, id, 2).await;
    assert_ne!(first, second);
    assert_eq!(
        events(&state, id, "permission.resolved").await.len(),
        1,
        "the second request is still open"
    );
    assert_eq!(provider.bodies().len(), 1, "the wake did not start a turn");

    allow(&state, id, second).await;
    wait_events(&state, id, "task.done", 2).await;
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 2);
    assert!(resolved
        .iter()
        .all(|e| e.payload["resolution"] == "allow_once"));
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "from w1");
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "from w2");
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 3);
    assert!(!bodies[1].contains("drift: look at the parser"));
    assert!(
        bodies[2].contains("drift: look at the parser"),
        "{}",
        bodies[2]
    );
}

// Given a write held for the Captain, bound to its canonical destination, and
//       a navigator wake posted while the request waits
// When the destination file is replaced by a symlink to another file before
//      the Captain allows the call
// Then the bound pathname no longer opens without following a link: nothing
//      is written through it, the call enters the decision again and asks a
//      fresh request, the held wake does not cancel it, and the wake runs only
//      after the turn ends
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destination_replaced_by_a_link_while_a_wake_is_held_is_decided_again() {
    let env = Env::new();
    let target = env.ws.join("notes.txt");
    let elsewhere = env.ws.join("elsewhere.txt");
    std::fs::write(&target, "original").unwrap();
    std::fs::write(&elsewhere, "keep").unwrap();
    let provider = provider(vec![
        one_write("w1", &target),
        done("written"),
        done("wake seen"),
    ])
    .await;
    let state = config(&env, &provider).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "update the notes").await;
    let first = nth_suspended_request(&state, id, 1).await;
    wake(&state, id, "drift: look at the parser").await;
    wait_events(&state, id, "message.received", 2).await;

    // The bound file becomes a link while the question waits.
    std::fs::remove_file(&target).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
    allow(&state, id, first).await;

    let second = nth_suspended_request(&state, id, 2).await;
    assert_ne!(first, second, "the moved destination is decided again");
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep");
    assert!(std::fs::symlink_metadata(&target)
        .unwrap()
        .file_type()
        .is_symlink());
    let posts = events(&state, id, "tool.post_call").await;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].payload["status"], "error");
    assert!(posts[0].payload["output"]
        .as_str()
        .unwrap()
        .contains("nothing was written"));
    assert_eq!(events(&state, id, "permission.resolved").await.len(), 1);
    assert_eq!(provider.bodies().len(), 1, "the wake did not start a turn");

    // Rejecting the fresh request ends the turn; the wake then runs.
    state
        .answer_permission(
            id,
            second,
            withnav::agent::permissions::PermissionDecision::Reject { message: None },
        )
        .await
        .unwrap();
    wait_events(&state, id, "task.done", 2).await;
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep");
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 3);
    assert!(
        bodies[2].contains("drift: look at the parser"),
        "{}",
        bodies[2]
    );
}
