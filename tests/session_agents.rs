//! Primary agents through the session service and the HTTP surface: the role
//! prompt on the wire, `/agent`, persistence, resume and `navai run --agent`.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use serde_json::{json, Value};

use withnav::bus::command::{Command, ContentBlock, DeliveryMode};
use withnav::bus::event::EventEnvelope;
use withnav::server::{app, AppState};

mod common;

#[tokio::test]
async fn successful_switch_fallback_warning_is_durable_and_replayed() {
    let env = env();
    let config = TestServerConfig::unreachable(&env.root).config();
    std::fs::write(
        env.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/mock-model"},"plan":{"model":"mock/missing"}}})
            .to_string(),
    )
    .unwrap();
    let state = AppState::new(config).unwrap();
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let result = state.run_command(id, "agent", "plan").await.unwrap();
    assert!(result.ok);
    assert!(result.data.unwrap()["warning"]
        .as_str()
        .unwrap()
        .contains("mock/missing"));
    state.history().flush().await.unwrap();
    let loaded = state.history().load(id).await.unwrap().unwrap();
    let event = loaded
        .envelopes
        .iter()
        .find(|e| e.kind == "model.changed")
        .unwrap();
    assert_eq!(event.payload["new"]["source"], "default_agent");
    assert!(event.payload["warning"].as_str().unwrap().contains("plan"));
    assert_eq!(event.payload["new"]["agent"], "plan");
}

#[tokio::test]
async fn creation_assignment_off_named_overrides_and_fallback_notice_are_durable() {
    let env = env();
    let config = TestServerConfig::unreachable(&env.root)
        .variants(json!({"low":{"reasoning_effort":"low"}}), Some("low"))
        .config();
    let state = AppState::new(config).unwrap();
    for off in [json!(null), json!("off")] {
        std::fs::write(
            env.root.join("cfg/config.json"),
            json!({"agents":{"build":{"model":"mock/mock-model","variant":off}}}).to_string(),
        )
        .unwrap();
        let id = state.create_session(&env.ws, None, false).await.unwrap();
        assert_eq!(
            state.history().meta(id).await.unwrap().unwrap().variant,
            None
        );
        let named = state
            .create_session_with(
                &env.ws,
                None,
                false,
                withnav::session::VariantChoice::Exact("low".into()),
                None,
            )
            .await
            .unwrap();
        let meta = state.history().meta(named).await.unwrap().unwrap();
        assert_eq!(meta.agent_overrides["build"].model, None);
        assert_eq!(
            meta.agent_overrides["build"].variant,
            withnav::session::store::AgentVariantOverride::Named("low".into())
        );
    }
    std::fs::write(
        env.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/mock-model"},"plan":{"model":"mock/absent"}}})
            .to_string(),
    )
    .unwrap();
    let id = state
        .create_session_with(
            &env.ws,
            None,
            false,
            withnav::session::VariantChoice::Default,
            Some("plan"),
        )
        .await
        .unwrap();
    state.history().flush().await.unwrap();
    let loaded = state.history().load(id).await.unwrap().unwrap();
    assert!(loaded.envelopes.iter().any(|e| e.kind == "command.result"
        && e.payload["data"]["warning"]
            .as_str()
            .is_some_and(|s| s.contains("plan") && s.contains("mock/absent"))));
    let rows_before = state
        .history()
        .list(&env.ws.display().to_string())
        .await
        .unwrap()
        .len();
    std::fs::write(
        env.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/absent"}}}).to_string(),
    )
    .unwrap();
    assert!(state.create_session(&env.ws, None, false).await.is_err());
    assert_eq!(
        state
            .history()
            .list(&env.ws.display().to_string())
            .await
            .unwrap()
            .len(),
        rows_before
    );
}

#[tokio::test]
async fn live_agent_switch_restores_model_and_off_override_after_restart() {
    let env = env();
    let config = TestServerConfig::unreachable(&env.root)
        .extra_alias("plan-key", "plan-model")
        .variants(json!({"low":{"reasoning_effort":"low"}}), Some("low"))
        .config();
    std::fs::write(
        env.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/mock-model"},"plan":{"model":"mock/plan-model"}}})
            .to_string(),
    )
    .unwrap();
    let state = AppState::new(config.clone()).unwrap();
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    assert!(
        state
            .run_command(id, "model", "mock/plan-model")
            .await
            .unwrap()
            .ok
    );
    assert!(state.run_command(id, "variant", "off").await.unwrap().ok);
    assert!(state.run_command(id, "agent", "plan").await.unwrap().ok);
    let plan = state.session_summary(id).await.unwrap();
    assert_eq!(plan.agent, "plan");
    assert_eq!(plan.variant.as_deref(), Some("low"));
    assert!(state.run_command(id, "agent", "build").await.unwrap().ok);
    let build = state.session_summary(id).await.unwrap();
    assert_eq!(build.agent, "build");
    assert_eq!(build.model, "mock/plan-model");
    assert_eq!(build.variant, None);
    state.close_session(id).await.unwrap();
    let reopened = AppState::new(config).unwrap();
    reopened
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    let restored = reopened.session_summary(id).await.unwrap();
    assert_eq!(restored.agent, "build");
    assert_eq!(restored.model, "mock/plan-model");
    assert_eq!(restored.variant, None);
    let provenance = reopened
        .history()
        .meta(id)
        .await
        .unwrap()
        .unwrap()
        .provenance
        .unwrap();
    assert_eq!(
        provenance.model,
        withnav::session::store::ModelSource::Override
    );
    assert_eq!(
        provenance.variant,
        withnav::session::store::VariantSource::Override
    );
    assert!(
        reopened
            .run_command(id, "variant", "inherit")
            .await
            .unwrap()
            .ok
    );
    assert_eq!(
        reopened
            .session_summary(id)
            .await
            .unwrap()
            .variant
            .as_deref(),
        Some("low")
    );
}

#[tokio::test]
async fn configured_agent_fallback_header_precedence_and_invalid_defaults() {
    let env = env();
    let config = TestServerConfig::unreachable(&env.root)
        .extra_alias("second", "second-model")
        .config();
    write_agent(
        &env,
        "custom.md",
        "---\nmode: primary\nmodel: mock/second-model\n---\nCustom role\n",
    );
    let settings = env.root.join("cfg/config.json");
    std::fs::write(&settings,json!({"agents":{"build":{"model":"mock/mock-model"},"custom":{"model":"mock/mock-model"},"plan":{"model":"mock/missing"}}}).to_string()).unwrap();
    let state = AppState::new(config.clone()).unwrap();
    for agent in ["custom", "plan"] {
        let id = state
            .create_session_with(
                &env.ws,
                None,
                false,
                withnav::session::VariantChoice::Default,
                Some(agent),
            )
            .await
            .unwrap();
        assert_eq!(
            state.session_summary(id).await.unwrap().model,
            "mock/mock-model"
        );
    }
    std::fs::write(
        &settings,
        json!({"agents":{"build":{"model":"mock/mock-model"}}}).to_string(),
    )
    .unwrap();
    let header = state
        .create_session_with(
            &env.ws,
            None,
            false,
            withnav::session::VariantChoice::Default,
            Some("custom"),
        )
        .await
        .unwrap();
    assert_eq!(
        state.session_summary(header).await.unwrap().model,
        "mock/second-model"
    );
    let fallback = state
        .create_session_with(
            &env.ws,
            None,
            false,
            withnav::session::VariantChoice::Default,
            Some("plan"),
        )
        .await
        .unwrap();
    assert_eq!(
        state.session_summary(fallback).await.unwrap().model,
        "mock/mock-model"
    );
    for value in [
        json!({"default_agent":"missing","agents":{"build":{"model":"mock/mock-model"}}}),
        json!({"agents":{"build":{"model":"mock/missing"}}}),
    ] {
        std::fs::write(&settings, value.to_string()).unwrap();
        assert!(state.create_session(&env.ws, None, false).await.is_err());
    }
    std::fs::write(
        &settings,
        json!({"agents":{"build":{"model":"mock/mock-model"}}}).to_string(),
    )
    .unwrap();
    let mut models: Value =
        serde_json::from_str(&std::fs::read_to_string(env.root.join("models.json")).unwrap())
            .unwrap();
    models["models"]["default"]["api"] = json!("fim");
    std::fs::write(env.root.join("models.json"), models.to_string()).unwrap();
    assert!(state.create_session(&env.ws, None, false).await.is_err());
}

#[tokio::test]
async fn configured_agents_choose_models_and_persist_explicit_off_on_resume() {
    use withnav::session::{
        store::{AgentVariantOverride, HistoryStore},
        VariantChoice,
    };
    let env = env();
    let config = TestServerConfig::unreachable(&env.root)
        .extra_alias("plan-key", "plan-model")
        .variants(json!({"low":{"reasoning_effort":"low"}}), Some("low"))
        .config();
    std::fs::write(
        env.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/mock-model"},"plan":{"model":"mock/plan-model"}}})
            .to_string(),
    )
    .unwrap();
    let service = AppState::new(config.clone()).unwrap();
    let build = service.create_session(&env.ws, None, false).await.unwrap();
    assert_eq!(
        service.session_summary(build).await.unwrap().model,
        "mock/mock-model"
    );
    let plan = service
        .create_session_with(&env.ws, None, false, VariantChoice::Default, Some("plan"))
        .await
        .unwrap();
    assert_eq!(
        service.session_summary(plan).await.unwrap().model,
        "mock/plan-model"
    );
    std::fs::write(env.root.join("cfg/config.json"),json!({"default_agent":"plan","agents":{"build":{"model":"mock/mock-model"},"plan":{"model":"mock/plan-model"}}}).to_string()).unwrap();
    let default_plan = service.create_session(&env.ws, None, false).await.unwrap();
    assert_eq!(
        service.session_summary(default_plan).await.unwrap().model,
        "mock/plan-model"
    );
    std::fs::write(
        env.root.join("cfg/config.json"),
        json!({"agents":{"build":{"model":"mock/mock-model"},"plan":{"model":"mock/plan-model"}}})
            .to_string(),
    )
    .unwrap();
    let overridden = service
        .create_session_with(
            &env.ws,
            Some("mock/plan-model"),
            false,
            VariantChoice::Clear,
            None,
        )
        .await
        .unwrap();
    let store = HistoryStore::open(env.root.join("history.db")).unwrap();
    let meta = store.meta(overridden).await.unwrap().unwrap();
    assert_eq!(
        meta.agent_overrides["build"].model.as_deref(),
        Some("mock/plan-model")
    );
    assert_eq!(
        meta.agent_overrides["build"].variant,
        AgentVariantOverride::Off
    );
    assert_eq!(meta.variant, None);
    service.close_session(overridden).await.unwrap();
    let reopened = AppState::new(config).unwrap();
    reopened
        .create_or_resume_session(&env.ws, None, false, Some(overridden))
        .await
        .unwrap();
    assert_eq!(
        reopened.session_summary(overridden).await.unwrap().model,
        "mock/plan-model"
    );
    assert_eq!(store.meta(overridden).await.unwrap().unwrap().variant, None);
    let child = reopened
        .service()
        .create_child_session(overridden, "explore", "inspect")
        .await
        .unwrap();
    let child_meta = store.meta(child).await.unwrap().unwrap();
    assert_eq!(child_meta.model, "mock/plan-model");
    assert_eq!(child_meta.variant, None);
    std::fs::write(env.root.join("cfg/config.json"),json!({"agents":{"build":{"model":"mock/mock-model"},"explore":{"model":"mock/mock-model","variant":"low"}}}).to_string()).unwrap();
    let assigned_child = reopened
        .service()
        .create_child_session(overridden, "explore", "inspect own model")
        .await
        .unwrap();
    let own = store.meta(assigned_child).await.unwrap().unwrap();
    assert_eq!(own.model, "mock/mock-model");
    assert_eq!(own.variant.as_deref(), Some("low"));
    reopened.close_session(overridden).await.unwrap();
    let mut models: Value =
        serde_json::from_str(&std::fs::read_to_string(env.root.join("models.json")).unwrap())
            .unwrap();
    models["providers"]["mock"]["base_url"] = json!("http://127.0.0.1:2/v1");
    std::fs::write(env.root.join("models.json"), models.to_string()).unwrap();
    assert!(reopened
        .create_or_resume_session(&env.ws, None, false, Some(overridden))
        .await
        .is_err());
}
use common::server::{collect_until, request_json, TestServerConfig};
use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_chunk, CapturingProvider};

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    ws: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonical");
    let home = root.join("home");
    let ws = root.join("ws");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    Env {
        _tmp: tmp,
        root,
        home,
        ws,
    }
}

fn config(env: &Env, provider: &CapturingProvider) -> TestServerConfig {
    TestServerConfig::mock(&env.root, &provider.url).rules_home(&env.home)
}

fn state(env: &Env, provider: &CapturingProvider) -> AppState {
    config(env, provider).state()
}

async fn provider(count: usize) -> CapturingProvider {
    let finish = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
    spawn_capturing_provider(
        (0..count)
            .map(|_| http_sse(&sse_body(&[&text_chunk("ok"), finish])))
            .collect(),
    )
    .await
}

/// A custom agent file in Navai's config directory.
fn write_agent(env: &Env, name: &str, text: &str) {
    let dir = env.root.join("cfg/agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), text).unwrap();
}

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

/// `(role, content)` of the messages of the `n`th captured request.
fn sent(provider: &CapturingProvider, n: usize) -> Vec<(String, String)> {
    let body: Value = serde_json::from_str(&provider.bodies()[n]).expect("json body");
    body["messages"]
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

async fn events(state: &AppState, id: ulid::Ulid, kind: &str) -> Vec<EventEnvelope> {
    state.history().flush().await.expect("flush");
    let sub = state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        withnav::bus::ResumeOutcome::Replay(all) => {
            all.into_iter().filter(|e| e.kind == kind).collect()
        }
        other => panic!("expected replay, got {other:?}"),
    }
}

async fn stored_agent(state: &AppState, id: ulid::Ulid) -> Option<String> {
    state.history().flush().await.expect("flush");
    state
        .history()
        .meta(id)
        .await
        .expect("meta")
        .expect("row")
        .primary_agent
}

// Given a session with no agent named
// Then it runs as build: no role text, NULL in the store, `agent: build` in
//      the summary and the list
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_runs_as_build_by_default() {
    let env = env();
    let provider = provider(1).await;
    let state = state(&env, &provider);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    converse(&state, id, "hello").await;

    let roles: Vec<String> = sent(&provider, 0).into_iter().map(|(r, _)| r).collect();
    assert_eq!(roles, ["system", "user"], "build adds no role message");
    assert_eq!(state.session_summary(id).await.unwrap().agent, "build");
    assert_eq!(stored_agent(&state, id).await, None);
    let listed = state.list(&env.ws.display().to_string()).await.unwrap();
    assert_eq!(listed[0].agent, "build");
}

// Given a session created as plan
// Then the plan role is sent after the contract, the choice is stored, and a
//      resume in a fresh service keeps it
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plan_session_sends_its_role_and_keeps_it_across_a_resume() {
    let env = env();
    let provider = provider(2).await;
    let first = state(&env, &provider);
    let id = first
        .create_session_as(&env.ws, None, false, Some("plan"))
        .await
        .unwrap();
    converse(&first, id, "one").await;
    let request = sent(&provider, 0);
    assert_eq!(request[0].0, "system");
    assert_eq!(request[1].0, "system");
    assert!(request[1].1.contains("plan mode"), "{request:?}");
    assert!(request[1].1.contains("markdown"));
    assert_eq!(stored_agent(&first, id).await.as_deref(), Some("plan"));
    first.close_session(id).await.unwrap();
    drop(first);

    let second = state(&env, &provider);
    let (resumed, _) = second
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    assert_eq!(second.session_summary(resumed).await.unwrap().agent, "plan");
    converse(&second, id, "two").await;
    assert!(sent(&provider, 1)[1].1.contains("plan mode"));
}

// Given an unknown agent, a subagent-only one, and a valid custom one
// When a session is created
// Then only the primary-capable ones are accepted, and no row is left behind
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_primary_agents_can_start_a_session() {
    let env = env();
    let provider = provider(1).await;
    write_agent(
        &env,
        "scribe.md",
        "---\ndescription: Writes docs\n---\nYou write documentation.\n",
    );
    let state = state(&env, &provider);
    for bad in ["nope", "explore", "Plan"] {
        let err = state
            .create_session_as(&env.ws, None, false, Some(bad))
            .await
            .unwrap_err();
        assert!(
            matches!(err, withnav::error::ServerError::UnknownAgent { .. }),
            "{bad}: {err:?}"
        );
        assert!(err.to_string().contains("build, plan, scribe"), "{err}");
    }
    assert_eq!(state.session_count(), 0);
    assert!(state
        .list(&env.ws.display().to_string())
        .await
        .unwrap()
        .is_empty());

    let id = state
        .create_session_as(&env.ws, None, false, Some("scribe"))
        .await
        .unwrap();
    converse(&state, id, "hi").await;
    assert_eq!(sent(&provider, 0)[1].1, "You write documentation.");
}

// Given a session saved as an agent whose file is gone
// When it is resumed
// Then the resume fails with an explicit error and the row is untouched
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_agent_that_vanished_is_an_error_not_build() {
    let env = env();
    let provider = provider(1).await;
    write_agent(&env, "scribe.md", "---\ndescription: d\n---\nRole.\n");
    let first = state(&env, &provider);
    let id = first
        .create_session_as(&env.ws, None, false, Some("scribe"))
        .await
        .unwrap();
    first.close_session(id).await.unwrap();
    drop(first);
    std::fs::remove_file(env.root.join("cfg/agents/scribe.md")).unwrap();

    let second = state(&env, &provider);
    let err = second
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap_err();
    assert!(
        matches!(err, withnav::error::ServerError::SavedAgentUnavailable { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("scribe"));
    assert_eq!(stored_agent(&second, id).await.as_deref(), Some("scribe"));

    // A file that turned subagent-only is refused the same way.
    write_agent(&env, "scribe.md", "---\nmode: subagent\n---\nRole.\n");
    let third = state(&env, &provider);
    let err = third
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        withnav::error::ServerError::SavedAgentUnavailable { .. }
    ));
}

// Given a conversation whose row has no agent (written before the column)
// When it is resumed
// Then it runs as build
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_row_resumes_as_build() {
    let env = env();
    let provider = provider(1).await;
    let state = state(&env, &provider);
    let id = ulid::Ulid::new();
    state
        .history()
        .record_session(id, &env.ws.display().to_string(), "mock/mock-model", false)
        .await
        .unwrap();
    let (resumed, _) = state
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    assert_eq!(state.session_summary(resumed).await.unwrap().agent, "build");
}

// Given a conversation on build
// When /agent plan runs between turns
// Then session.agent_changed is published and stored, the choice is
//      persisted, the NEXT request carries the plan role, and the transcript
//      holds no role text
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_changes_the_next_request_and_nothing_in_the_transcript() {
    let env = env();
    let provider = provider(3).await;
    let state = state(&env, &provider);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    converse(&state, id, "first").await;
    let system_as_build = state.context(id).await.unwrap().breakdown.system;

    let out = state.run_command(id, "agent", "plan").await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(out.render_text(), "agent plan (was build)");
    assert_eq!(stored_agent(&state, id).await.as_deref(), Some("plan"));
    let changed = events(&state, id, "session.agent_changed").await;
    assert_eq!(changed.len(), 1);
    assert_eq!(
        changed[0].payload,
        json!({ "agent": "plan", "previous": "build" })
    );
    assert_eq!(state.session_summary(id).await.unwrap().agent, "plan");

    converse(&state, id, "second").await;
    // The estimate counts the role message the request carries.
    let system_as_plan = state.context(id).await.unwrap().breakdown.system;
    assert!(
        system_as_plan > system_as_build,
        "plan's role text is estimated: {system_as_build} -> {system_as_plan}"
    );
    let request = sent(&provider, 1);
    assert!(request[1].1.contains("plan mode"), "{request:?}");
    let roles: Vec<&str> = request.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(
        roles,
        ["system", "system", "user", "assistant", "user"],
        "the role is not in the history"
    );

    let out = state.run_command(id, "agent", "build").await.unwrap();
    assert!(out.ok);
    assert_eq!(stored_agent(&state, id).await.as_deref(), Some("build"));
    converse(&state, id, "third").await;
    let roles: Vec<String> = sent(&provider, 2).into_iter().map(|(r, _)| r).collect();
    assert_eq!(roles[1], "user", "build sends no role again: {roles:?}");
}

// Given a session
// When /agent has no argument, an unknown or subagent-only id, or too many
//      words
// Then it lists the choices or refuses, and publishes no change
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_lists_and_refuses_bad_choices() {
    let env = env();
    let provider = provider(1).await;
    let state = state(&env, &provider);
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    let out = state.run_command(id, "agent", "").await.unwrap();
    assert!(out.ok);
    let data = out.data.as_ref().unwrap();
    assert_eq!(data["agent"], "build");
    let ids: Vec<&str> = data["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["build", "plan"], "subagents are not offered");
    let text = out.render_text();
    assert!(
        text.starts_with("agent build\n") && text.contains("plan"),
        "{text}"
    );

    for (args, expected) in [
        ("nope", "unknown primary agent 'nope'"),
        ("explore", "unknown primary agent 'explore'"),
        ("plan extra", "usage: /agent <name>"),
    ] {
        let out = state.run_command(id, "agent", args).await.unwrap();
        assert!(!out.ok, "{args}");
        assert_eq!(out.error.unwrap(), expected, "{args}");
    }
    let same = state.run_command(id, "agent", "build").await.unwrap();
    assert!(same.ok);
    assert_eq!(same.data.unwrap()["changed"], false);
    assert!(events(&state, id, "session.agent_changed").await.is_empty());
    assert_eq!(stored_agent(&state, id).await, None);
}

// Given a session waiting on its `ask`
// When /agent plan runs
// Then it is refused like every command and nothing changes
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_during_a_pending_ask_is_refused() {
    let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"which scope?\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[ask]))]).await;
    let env = env();
    let state = state(&env, &provider);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();
    state
        .send(Command {
            command_id: ulid::Ulid::new(),
            session_id: id,
            source_actor_id: "user:test".to_string(),
            target_actor_id: "agent:main".to_string(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text { text: "go".into() }],
            created_at: std::time::SystemTime::now(),
        })
        .await
        .unwrap();
    collect_until(sub, |e| {
        e.kind == "agent.state" && e.payload["to"] == "yielded"
    })
    .await;

    let out = state.run_command(id, "agent", "plan").await.unwrap();
    assert!(!out.ok);
    assert!(out.error.unwrap().contains("waiting for an answer"));
    assert_eq!(state.session_summary(id).await.unwrap().agent, "build");
    assert_eq!(stored_agent(&state, id).await, None);
}

// Given the HTTP surface
// When a session is created with an agent
// Then GET and the list show it, an unknown agent is a 400, and a resume
//      ignores the agent field
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_creates_with_an_agent_and_shows_it() {
    let env = env();
    let provider = provider(1).await;
    let state = state(&env, &provider);
    let router = app(state.clone());
    let ws = env.ws.display().to_string();

    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({ "workspace": ws, "agent": "plan" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["session_id"].as_str().unwrap().to_string();

    let (_, summary) = request_json(&router, "GET", &format!("/v1/sessions/{id}"), None).await;
    assert_eq!(summary["agent"], "plan");
    let (_, list) = request_json(
        &router,
        "GET",
        &format!("/v1/sessions?workspace={ws}"),
        None,
    )
    .await;
    assert_eq!(list["sessions"][0]["agent"], "plan");

    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({ "workspace": ws, "agent": "wizard" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("wizard"));

    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({ "workspace": ws, "resume": id, "agent": "build" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = request_json(
        &router,
        "POST",
        "/v1/sessions",
        Some(&json!({ "workspace": ws, "resume": id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, summary) = request_json(&router, "GET", &format!("/v1/sessions/{id}"), None).await;
    assert_eq!(summary["agent"], "plan", "a resume keeps the stored agent");

    // /agent over HTTP.
    let (status, body) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/commands"),
        Some(&json!({ "name": "agent", "arguments": "build" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["previous"], "plan");
}

fn run_navai(root: &Path, models: &Path, ws: &Path, extra: &[&str]) -> std::process::Output {
    std::fs::create_dir_all(root.join("config/navai")).unwrap();
    std::fs::write(
        root.join("config/navai/config.json"),
        r#"{"agents":{"build":{"model":"mock/mock-model"},"plan":{"model":"mock/mock-model"}}}"#,
    )
    .unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_nav"))
        .args(["run", "--workspace"])
        .arg(ws)
        .arg("--models")
        .arg(models)
        .args(extra)
        .env("NAVAI_TEST_KEY", "test-key")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .output()
        .expect("run the binary")
}

// Given `navai run --agent plan`
// Then the request carries the plan role; an unknown agent exits 1 with the
//      choices
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navai_run_agent_selects_the_role() {
    let env = env();
    let provider = provider(1).await;
    let cfg = config(&env, &provider);
    cfg.config();
    let models = cfg.models_path();
    let (root, ws) = (env.root.clone(), env.ws.clone());
    let (ok, bad) = tokio::task::spawn_blocking(move || {
        (
            run_navai(
                &root,
                &models,
                &ws,
                &["--agent", "plan", "--prompt", "hello"],
            ),
            run_navai(
                &root,
                &models,
                &ws,
                &["--agent", "wizard", "--prompt", "hello"],
            ),
        )
    })
    .await
    .unwrap();
    assert_eq!(ok.status.code(), Some(0), "{ok:?}");
    let request = sent(&provider, 0);
    assert!(request[1].1.contains("plan mode"), "{request:?}");
    assert_eq!(bad.status.code(), Some(1), "{bad:?}");
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(stderr.contains("unknown agent 'wizard'"), "{stderr}");
}

// Given a session created as plan, with no message sent
// Then the gauge already counts plan's role
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_plan_session_counts_its_role_before_any_turn() {
    let env = env();
    let provider = provider(1).await;
    let state = state(&env, &provider);
    let as_build = state.create_session(&env.ws, None, false).await.unwrap();
    let as_plan = state
        .create_session_as(&env.ws, None, false, Some("plan"))
        .await
        .unwrap();
    let build = state.context(as_build).await.unwrap().breakdown.system;
    let plan = state.context(as_plan).await.unwrap().breakdown.system;
    assert!(plan > build, "plan's role is estimated: {build} vs {plan}");
}

// Given a session before its first turn
// When /agent plan then /agent build run
// Then the gauge grows with plan's role, publishes it live, and shrinks back
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_agents_moves_the_gauge_without_a_turn() {
    let env = env();
    let provider = provider(1).await;
    let state = state(&env, &provider);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let before = state.context(id).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();

    assert!(state.run_command(id, "agent", "plan").await.unwrap().ok);
    let as_plan = state.context(id).await.unwrap();
    assert!(as_plan.breakdown.system > before.breakdown.system);
    assert_ne!(as_plan.incarnation, before.incarnation);

    assert!(state.run_command(id, "agent", "build").await.unwrap().ok);
    let back = state.context(id).await.unwrap();
    assert_eq!(back.breakdown.system, before.breakdown.system);

    // `context.updated` is live-only: read it from the subscription.
    let live = collect_until(sub, |e| {
        e.kind == "session.agent_changed" && e.payload["agent"] == "build"
    })
    .await;
    assert!(
        live.iter().any(|e| e.kind == "context.updated"
            && e.payload["breakdown"]["system"] == as_plan.breakdown.system),
        "context.updated carries the plan estimate"
    );
}

// Given a session created as plan
// Then its row holds plan as soon as creation returns: no flush, no turn
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_row_holds_the_agent_as_soon_as_creation_returns() {
    let env = env();
    let provider = provider(1).await;
    let state = state(&env, &provider);
    let id = state
        .create_session_as(&env.ws, None, false, Some("plan"))
        .await
        .unwrap();
    let row = state.history().meta(id).await.unwrap().expect("row");
    assert_eq!(row.primary_agent.as_deref(), Some("plan"));
}
