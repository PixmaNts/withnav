//! Skills through the session service: a scripted provider captures the
//! requests, so the catalog's place on the wire, the `skill` call and the
//! result's survival across a resume are all checked end to end. The home
//! directory is a temporary one in every test.

use std::path::PathBuf;

use serde_json::{json, Value};

use withnav::bus::command::{Command, ContentBlock, DeliveryMode};
use withnav::server::AppState;

mod common;

#[test]
fn message_received_type_roundtrips_optional_driver_selection() {
    use withnav::bus::event::MessageReceived;
    let mut payload = json!({"command_id":ulid::Ulid::new(), "source":"user:http", "target":"agent:main", "mode":"next-turn", "content":"driver text"});
    let legacy: MessageReceived = serde_json::from_value(payload.clone()).unwrap();
    assert!(legacy.selected_skill.is_none());
    assert!(serde_json::to_value(legacy)
        .unwrap()
        .get("selected_skill")
        .is_none());
    payload["selected_skill"] = json!({"name":"review","origin":"captain"});
    let selected: MessageReceived = serde_json::from_value(payload.clone()).unwrap();
    assert_eq!(selected.selected_skill.as_ref().unwrap().name, "review");
    assert_eq!(serde_json::to_value(selected).unwrap(), payload);
}
use common::server::{collect_until, TestServerConfig};
use common::sse::CapturingProvider;
use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_chunk, tool_call_chunk};

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    ws: PathBuf,
}

/// A temp home with a nested skill library and a rules file, and a Git
/// workspace with one project skill.
fn env() -> Env {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonical");
    let home = root.join("home");
    let ws = root.join("ws");
    let skills = home.join(".agents/skills");
    std::fs::create_dir_all(skills.join("matt/ask-matt")).unwrap();
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::create_dir_all(ws.join(".agents/skills/local")).unwrap();
    std::fs::write(home.join(".agents/AGENTS.md"), "global rule").unwrap();
    std::fs::write(skills.join("AGENTS.md"), "not a skill").unwrap();
    std::fs::write(
        skills.join("matt/ask-matt/SKILL.md"),
        "---\nname: ask-matt\ndescription: >\n  Ask Matt\n  about it.\nversion: 3\n---\n\nSTEP ONE: ask Matt.\n",
    )
    .unwrap();
    std::fs::write(skills.join("matt/ask-matt/checklist.md"), "- item").unwrap();
    std::fs::write(
        ws.join(".agents/skills/local/SKILL.md"),
        "---\nname: local-flow\ndescription: The project's own flow.\n---\nLocal body.\n",
    )
    .unwrap();
    Env {
        _tmp: tmp,
        root,
        home,
        ws,
    }
}

fn state(env: &Env, provider: &CapturingProvider, alias: &str, navigator: bool) -> AppState {
    let config = TestServerConfig::mock(&env.root, &provider.url)
        .model_alias(alias, "mock-model")
        .rules_home(&env.home);
    if navigator {
        config.assigned_navigator_state()
    } else {
        config.state()
    }
}

fn finish() -> String {
    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_string()
}

fn answer(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text), &finish()]))
}

fn skill_call(name: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&tool_call_chunk(
        "call-1",
        "skill",
        &json!({ "name": name }),
    )]))
}

/// Sends `text` and waits for the turn to end.
async fn converse(state: &AppState, id: ulid::Ulid, text: &str) {
    let sub = state.subscribe(id, None).await.expect("subscribe");
    state
        .send(Command {
            command_id: ulid::Ulid::new(),
            session_id: id,
            source_actor_id: "user:test".to_string(),
            target_actor_id: "agent:main".to_string(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            created_at: std::time::SystemTime::now(),
        })
        .await
        .expect("send");
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
}

fn body(provider: &CapturingProvider, n: usize) -> Value {
    serde_json::from_str(&provider.bodies()[n]).expect("json body")
}

/// `(role, content)` of the messages of the `n`th captured request.
fn messages(provider: &CapturingProvider, n: usize) -> Vec<(String, String)> {
    body(provider, n)["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap_or_default().to_string(),
                m["content"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn tool_names(provider: &CapturingProvider, n: usize) -> Vec<String> {
    body(provider, n)["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| {
                    t["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_selection_loads_the_skill_before_the_first_model_request() {
    let env = env();
    let provider = spawn_capturing_provider(vec![answer("done")]).await;
    let state = state(&env, &provider, "default", false);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let router = withnav::server::app(state.clone());
    let (status, listed) =
        common::server::request_json(&router, "GET", &format!("/v1/sessions/{id}/skills"), None)
            .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(listed["skills"][0]["name"], "local-flow");
    assert_eq!(listed["skills"][1]["description"], "Ask Matt about it.");
    let sub = state.subscribe(id, None).await.unwrap();
    let (status, _) = common::server::request_json(&router, "POST", &format!("/v1/sessions/{id}/messages"), Some(&json!({
        "target_actor_id":"agent:main", "content":[{"type":"text","text":"follow this workflow"},{"type":"skill","name":"ask-matt"}]
    }))).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let events = collect_until(sub, |event| event.kind == "turn.end").await;
    let selected = events
        .iter()
        .find(|event| event.kind == "message.received")
        .unwrap();
    assert_eq!(
        selected.payload["selected_skill"],
        json!({"name":"ask-matt","origin":"captain"})
    );
    let withnav::bus::ResumeOutcome::Replay(replay) = state.subscribe(id, None).await.unwrap().replay
    else {
        panic!("durable replay")
    };
    assert_eq!(
        replay
            .iter()
            .find(|event| event.kind == "message.received")
            .unwrap()
            .payload,
        selected.payload
    );
    let (address, server) = common::server::spawn_server(state.clone()).await;
    let response = reqwest::get(format!("http://{address}/v1/sessions/{id}/events"))
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    use futures::StreamExt;
    let mut sse = response.bytes_stream();
    let mut bytes = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(chunk) = sse.next().await {
            bytes.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if bytes.contains("selected_skill") {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(bytes.contains("ask-matt"));
    assert!(bytes.contains("\"origin\":\"driver\""));
    server.abort();
    for stream in [&events, &replay] {
        let mut projection = withnav::tui::state::State::default();
        for envelope in stream
            .iter()
            .filter(|event| event.kind == "message.received")
        {
            withnav::tui::reducer::update(
                &mut projection,
                withnav::tui::protocol::ClientEvent::Envelope {
                    generation: Default::default(),
                    envelope: envelope.clone(),
                },
            )
            .unwrap();
        }
        assert!(projection.conversation.blocks.iter().any(|block| matches!(&block.kind, withnav::tui::state::BlockKind::Notice(text) if text.contains("skill: ask-matt"))));
    }
    let sent = body(&provider, 0);
    let messages = sent["messages"].as_array().unwrap();
    assert!(messages.iter().any(|message| message["role"] == "user"
        && message["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("follow this workflow\n\n"))));
    assert!(!messages
        .iter()
        .any(|message| message["role"] == "tool" || message.get("tool_calls").is_some()));
    let selected_message = messages
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    let encoded = selected_message
        .split("<driver_selected_skill encoding=\"json\">\n")
        .nth(1)
        .unwrap()
        .split("\n</driver_selected_skill>")
        .next()
        .unwrap();
    let loaded: Value = serde_json::from_str(encoded).unwrap();
    assert_eq!(loaded["name"], "ask-matt");
    assert_eq!(loaded["selected_by"], "captain");
    assert!(loaded["content"]
        .as_str()
        .unwrap()
        .contains("STEP ONE: ask Matt."));
    assert!(loaded["content"].as_str().unwrap().contains("checklist.md"));
    let (_, stored, _) = state
        .history()
        .load_for_resume(selected.session_id)
        .await
        .unwrap()
        .unwrap();
    let restored: Vec<withnav::provider::openai::ChatMessage> =
        serde_json::from_str(&serde_json::to_string(&stored).unwrap()).unwrap();
    assert!(restored.iter().any(|message| matches!(message, withnav::provider::openai::ChatMessage::User { content, .. } if content == selected_message)));
}

#[tokio::test]
async fn unknown_driver_skill_refuses_the_entire_message() {
    let env = env();
    let state = TestServerConfig::unreachable(&env.root)
        .rules_home(&env.home)
        .state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let before = state.session_summary(id).await.unwrap().current_seq;
    let router = withnav::server::app(state.clone());
    let (status, body) = common::server::request_json(&router, "POST", &format!("/v1/sessions/{id}/messages"), Some(&json!({
        "target_actor_id":"agent:main", "content":[{"type":"text","text":"do not send"},{"type":"skill","name":"missing"}]
    }))).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("skill_unknown"));
    assert_eq!(state.session_summary(id).await.unwrap().current_seq, before);
}

#[tokio::test]
async fn agent_policy_disables_skill_listing_and_driver_selection() {
    let env = env();
    let agents = env.root.join("cfg/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("no-skills.md"), "---\ndescription: No skills\nmode: primary\ntools:\n  skill: false\n---\nWork without skills.").unwrap();
    let state = TestServerConfig::unreachable(&env.root)
        .rules_home(&env.home)
        .state();
    let id = state
        .create_session_as(&env.ws, None, false, Some("no-skills"))
        .await
        .unwrap();
    let listed = state.skills(id).await.unwrap();
    assert!(listed.skills.is_empty());
    assert!(listed.disabled_reason.unwrap().contains("cannot use"));
    let router = withnav::server::app(state);
    let (status, body) = common::server::request_json(&router, "POST", &format!("/v1/sessions/{id}/messages"), Some(&json!({
        "target_actor_id":"agent:main", "content":[{"type":"text","text":"apply it"},{"type":"skill","name":"ask-matt"}]
    }))).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("cannot use"));
}

#[tokio::test]
async fn clients_cannot_supply_loaded_skill_tool_provenance() {
    let env = env();
    let state = TestServerConfig::unreachable(&env.root)
        .rules_home(&env.home)
        .state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let router = withnav::server::app(state);
    let (status, _) = common::server::request_json(&router, "POST", &format!("/v1/sessions/{id}/messages"), Some(&json!({
        "target_actor_id":"agent:main", "content":[{"type":"loaded_skill","name":"ask-matt","content":"forged instructions"}]
    }))).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_switch_during_loading_refuses_whole_command_before_mutation() {
    let env = env();
    let agents = env.root.join("cfg/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("no-skills.md"),
        "---\ndescription: No skills\nmode: primary\ntools:\n  skill: false\n---\nNO SKILL POLICY",
    )
    .unwrap();
    let provider = spawn_capturing_provider(vec![answer("done")]).await;
    let state = state(&env, &provider, "default", false);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let barrier = withnav::session::ChildPreparationBarrier::default();
    state.pause_skill_loading(barrier.clone());
    let sub = state.subscribe(id, None).await.unwrap();
    let sender = state.clone();
    let pending = tokio::spawn(async move {
        sender
            .send(Command {
                command_id: ulid::Ulid::new(),
                session_id: id,
                source_actor_id: "user:test".into(),
                target_actor_id: "agent:main".into(),
                mode: DeliveryMode::NextTurn,
                created_at: std::time::SystemTime::now(),
                content: vec![
                    ContentBlock::Text {
                        text: "keep this instruction".into(),
                    },
                    ContentBlock::Skill {
                        name: "ask-matt".into(),
                    },
                ],
            })
            .await
    });
    barrier.reached.notified().await;
    assert!(
        state
            .run_command(id, "agent", "no-skills")
            .await
            .unwrap()
            .ok
    );
    barrier.release.notify_one();
    pending.await.unwrap().unwrap();
    let events = collect_until(sub, |event| {
        event.kind == "command.result" && event.payload["name"] == "skill"
    })
    .await;
    let refused = events
        .iter()
        .find(|event| event.kind == "command.result" && event.payload["name"] == "skill")
        .unwrap();
    let received = events
        .iter()
        .find(|event| event.kind == "message.received")
        .unwrap();
    assert_eq!(
        refused.payload["operation_id"],
        received.payload["command_id"]
    );
    assert_eq!(
        refused.correlation_id.unwrap().to_string(),
        received.payload["command_id"].as_str().unwrap()
    );
    assert!(provider.requests().is_empty());
    converse(&state, id, "allowed instruction").await;
    let sent = messages(&provider, 0);
    assert!(sent
        .iter()
        .any(|(_, text)| text.contains("NO SKILL POLICY")));
    assert!(sent.iter().any(|(_, text)| text == "allowed instruction"));
    assert!(!sent
        .iter()
        .any(|(_, text)| text.contains("keep this instruction") || text.contains("STEP ONE")));
    let (_, stored, _) = state.history().load_for_resume(id).await.unwrap().unwrap();
    assert!(!serde_json::to_string(&stored)
        .unwrap()
        .contains("keep this instruction"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_selection_does_not_replace_resumed_or_inherited_driver_task() {
    let env = env();
    let agents = env.root.join("cfg/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("no-skills.md"),
        "---\ndescription: No skills\nmode: primary\ntools:\n  skill: false\n---\nNo skills.",
    )
    .unwrap();
    let provider =
        spawn_capturing_provider(vec![answer("original done"), answer("child done")]).await;
    let state = state(&env, &provider, "default", false);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    converse(&state, id, "previous real driver task").await;
    let barrier = withnav::session::ChildPreparationBarrier::default();
    state.pause_skill_loading(barrier.clone());
    let sub = state.subscribe(id, None).await.unwrap();
    let sender = state.clone();
    let pending = tokio::spawn(async move {
        sender
            .send(Command {
                command_id: ulid::Ulid::new(),
                session_id: id,
                source_actor_id: "user:test".into(),
                target_actor_id: "agent:main".into(),
                mode: DeliveryMode::NextTurn,
                created_at: std::time::SystemTime::now(),
                content: vec![
                    ContentBlock::Text {
                        text: "REFUSED TASK".into(),
                    },
                    ContentBlock::Skill {
                        name: "ask-matt".into(),
                    },
                ],
            })
            .await
    });
    barrier.reached.notified().await;
    assert!(
        state
            .run_command(id, "agent", "no-skills")
            .await
            .unwrap()
            .ok
    );
    barrier.release.notify_one();
    pending.await.unwrap().unwrap();
    collect_until(sub, |event| {
        event.kind == "command.result" && event.payload["name"] == "skill"
    })
    .await;
    state.close_session(id).await.unwrap();
    state
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    assert_eq!(
        state.driver_task_probe(id).as_deref(),
        Some("previous real driver task")
    );
    assert_eq!(
        state
            .history()
            .latest_driver_message(id)
            .await
            .unwrap()
            .as_deref(),
        Some("previous real driver task")
    );
    let child = state
        .create_child_session(id, "explore", "delegated work")
        .await
        .unwrap();
    assert_eq!(
        state.driver_task_probe(child).as_deref(),
        Some("previous real driver task")
    );
}

// Given a home library (nested) and a project skill, plus a rules file
// When a session is created, a message is sent, the model calls `skill`
// Then the request is: contract, rules, CATALOG, user; the catalog lists the
//      project skill first; the second request carries the body as the tool
//      result, and `skill` is among the tools
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_catalog_follows_the_rules_and_the_skill_call_returns_the_body() {
    let env = env();
    let provider = spawn_capturing_provider(vec![skill_call("ask-matt"), answer("done")]).await;
    let state = state(&env, &provider, "default", false);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    converse(&state, id, "hello").await;

    let sent = messages(&provider, 0);
    let roles: Vec<&str> = sent.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(roles, ["system", "system", "system", "user"], "{sent:?}");
    assert!(sent[1].1.contains("global rule"), "rules come first");
    let catalog = &sent[2].1;
    assert!(catalog.contains("skill({\"name\":\"<id>\"})"), "{catalog}");
    let local = catalog
        .find("local-flow — The project's own flow.")
        .expect("project skill");
    let matt = catalog
        .find("ask-matt — Ask Matt about it.")
        .expect("nested skill");
    assert!(local < matt, "project skills first: {catalog}");
    assert!(!catalog.contains("not a skill"));
    assert!(tool_names(&provider, 0).contains(&"skill".to_string()));

    let second = messages(&provider, 1);
    let (role, content) = second.last().unwrap();
    assert_eq!(role, "tool");
    assert!(content.contains("STEP ONE: ask Matt."), "{content}");
    assert!(content.contains("SKILL.md"));
    assert!(content.contains("- checklist.md"));
    assert!(
        !content.contains("version: 3"),
        "the header is not repeated"
    );
    // The catalog is not in the history, it is re-sent as its own message.
    assert_eq!(
        second
            .iter()
            .filter(|(_, c)| c.contains("local-flow —"))
            .count(),
        1
    );
}

// Given a skill loaded in a first session
// When the conversation is closed and resumed in a fresh service
// Then the tool result is still in the history, and there is ONE catalog
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_loaded_skill_survives_a_resume() {
    let env = env();
    let provider =
        spawn_capturing_provider(vec![skill_call("ask-matt"), answer("one"), answer("two")]).await;
    let first = state(&env, &provider, "default", false);
    let id = first.create_session(&env.ws, None, false).await.unwrap();
    converse(&first, id, "load it").await;
    first.close_session(id).await.expect("close");
    drop(first);

    let second = state(&env, &provider, "default", false);
    let (resumed, was_resumed) = second
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .expect("resume");
    assert!(was_resumed && resumed == id);
    converse(&second, id, "again").await;

    let sent = messages(&provider, 2);
    let roles: Vec<&str> = sent.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(
        roles,
        [
            "system",
            "system",
            "system",
            "user",
            "assistant",
            "tool",
            "assistant",
            "user"
        ],
        "{sent:?}"
    );
    assert!(
        sent[2].1.contains("ask-matt —"),
        "the catalog is rebuilt on resume"
    );
    assert!(
        sent[5].1.contains("STEP ONE: ask Matt."),
        "the tool result is kept"
    );
    assert!(tool_names(&provider, 2).contains(&"skill".to_string()));
}

// Given a live session, then a skill added and another one's description
//       changed on disk
// When the next message is sent, the conversation closed and resumed in a
//      fresh service, and a message sent again
// Then the live catalog did not change (a snapshot), and the resumed one
//      shows the new skill and the new description, once
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_rediscovers_the_skills_a_live_session_does_not() {
    let env = env();
    let provider =
        spawn_capturing_provider(vec![answer("one"), answer("two"), answer("three")]).await;
    let first = state(&env, &provider, "default", false);
    let id = first.create_session(&env.ws, None, false).await.unwrap();
    converse(&first, id, "one").await;
    let created = messages(&provider, 0)[2].1.clone();
    assert!(
        created.contains("ask-matt — Ask Matt about it."),
        "{created}"
    );
    assert!(!created.contains("late-flow"));

    std::fs::create_dir_all(env.ws.join(".agents/skills/late")).unwrap();
    std::fs::write(
        env.ws.join(".agents/skills/late/SKILL.md"),
        "---\nname: late-flow\ndescription: Added while running.\n---\nLate body.\n",
    )
    .unwrap();
    std::fs::write(
        env.home.join(".agents/skills/matt/ask-matt/SKILL.md"),
        "---\nname: ask-matt\ndescription: Asks Matt, reworded.\n---\nSTEP ONE.\n",
    )
    .unwrap();
    converse(&first, id, "two").await;
    assert_eq!(
        messages(&provider, 1)[2].1,
        created,
        "the live catalog is a snapshot"
    );
    first.close_session(id).await.expect("close");
    drop(first);

    let second = state(&env, &provider, "default", false);
    let (resumed, was_resumed) = second
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .expect("resume");
    assert!(was_resumed && resumed == id);
    converse(&second, id, "three").await;

    let sent = messages(&provider, 2);
    let catalog = &sent[2].1;
    assert!(
        catalog.contains("late-flow — Added while running."),
        "{catalog}"
    );
    assert!(
        catalog.contains("ask-matt — Asks Matt, reworded."),
        "{catalog}"
    );
    assert!(!catalog.contains("Ask Matt about it."), "{catalog}");
    assert_eq!(
        sent.iter()
            .filter(|(_, c)| c.contains("late-flow —"))
            .count(),
        1,
        "{sent:?}"
    );
}

// Given a home and a workspace without any skill
// Then there is no catalog message and no `skill` tool
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_skills_the_request_is_unchanged() {
    let env = env();
    std::fs::remove_dir_all(env.home.join(".agents/skills")).unwrap();
    std::fs::remove_dir_all(env.ws.join(".agents")).unwrap();
    let provider = spawn_capturing_provider(vec![answer("ok")]).await;
    let state = state(&env, &provider, "default", false);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    converse(&state, id, "hello").await;
    let roles: Vec<String> = messages(&provider, 0).into_iter().map(|(r, _)| r).collect();
    assert_eq!(roles, ["system", "system", "user"]);
    assert!(!tool_names(&provider, 0).contains(&"skill".to_string()));
}

// Given a navigator session
// Then the order is: contract, role, rules, catalog, user, and the navigator
//      still has its `intent` tool next to `skill`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_navigator_catalog_comes_after_the_rules() {
    let env = env();
    let provider = spawn_capturing_provider(vec![answer("ok")]).await;
    let state = state(&env, &provider, "cognition", true);
    let id = state
        .create_session(&env.ws, None, true)
        .await
        .expect("create");
    converse(&state, id, "hello").await;

    let sent = messages(&provider, 0);
    let roles: Vec<&str> = sent.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(
        roles,
        ["system", "system", "system", "system", "user"],
        "{sent:?}"
    );
    assert_eq!(sent[1].1, withnav::navigator::COGNITION_PROMPT);
    assert!(sent[2].1.contains("global rule"));
    assert!(sent[3].1.contains("ask-matt —"));
    let tools = tool_names(&provider, 0);
    assert!(tools.contains(&"intent".to_string()) && tools.contains(&"skill".to_string()));
}

// Given a call to an unknown skill
// Then the model gets a structured error, the turn still ends
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_skill_is_a_structured_error_for_the_model() {
    let env = env();
    let provider = spawn_capturing_provider(vec![skill_call("nope"), answer("ok")]).await;
    let state = state(&env, &provider, "default", false);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    converse(&state, id, "hello").await;
    let second = messages(&provider, 1);
    let (role, content) = second.last().unwrap();
    assert_eq!(role, "tool");
    assert!(content.contains("skill_unknown"), "{content}");
}

// Given a home with one rules file and a workspace with two skills
// When the session's context is read
// Then it counts the rule file and the skills the catalog lists, and says it is an estimate
#[tokio::test]
async fn the_context_snapshot_counts_the_rule_files_and_listed_skills() {
    let env = env();
    let provider = spawn_capturing_provider(vec![]).await;
    let state = state(&env, &provider, "default", false);
    let id = state
        .create_session(&env.ws, None, false)
        .await
        .expect("create");
    let snap = serde_json::to_value(state.context(id).await.expect("context")).unwrap();
    assert_eq!(snap["estimated"], true);
    assert_eq!(snap["rule_files"], 1);
    assert_eq!(snap["skills"], json!({"listed": 2, "omitted": 0}));
    assert!(snap["breakdown"]["rules"].as_u64().unwrap() > 0);
    assert!(snap["breakdown"]["skills_catalog"].as_u64().unwrap() > 0);
}
