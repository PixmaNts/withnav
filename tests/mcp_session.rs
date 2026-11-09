//! MCP tools inside sessions: shutdown order, subagent inventories and `nav run` startup.
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use withnav::server::{AppState, ServerConfig};

mod common;
use common::server::{TestServerConfig, captain_command, wait_for};
use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_response, tool_calls_chunk};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py");

fn with_mcp(cfg: &ServerConfig, server: Value, yolo: bool) {
    let mut settings = withnav::settings::load(&cfg.config_dir).unwrap();
    settings.set_dotted("mcp", json!({ "fake": server }));
    settings.set_dotted("yolo", json!(yolo));
    withnav::settings::save(&cfg.config_dir, &settings).unwrap();
}

async fn ready(state: &AppState) {
    wait_for("MCP discovery", || {
        state
            .service()
            .mcp_statuses()
            .iter()
            .any(|s| s.server_id == "fake" && s.state == "ready")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_sessions_before_mcp_so_a_queued_call_never_runs() {
    let env = common::TestEnv::new();
    let target = env.ws.join("queued.txt");
    let provider = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[&tool_calls_chunk(&[
            ("slow", "mcp__fake__slow", json!({})),
            (
                "queued",
                "write",
                json!({"path": target, "content": "must not exist"}),
            ),
        ])])),
        text_response("done"),
    ])
    .await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.home)
        .config();
    with_mcp(
        &cfg,
        json!({"command":[common::python(),FIXTURE],"env":{"SLOW_SECONDS":"5"}}),
        true,
    );
    let state = AppState::new(cfg).unwrap();
    ready(&state).await;
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    state
        .send(captain_command(id, "slow then write".into()))
        .await
        .unwrap();
    wait_for("the slow MCP call", || !provider.bodies().is_empty()).await;
    // The provider answered; give the actor time to dispatch the slow call.
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::timeout(Duration::from_secs(15), state.service().close_all())
        .await
        .expect("shutdown is bounded");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!target.exists(), "the queued call ran during MCP shutdown");
}

/// A writable subagent with `mcp_child.md` and `mcp_denied.md` definitions.
fn install_agents(ws: &Path) {
    let dir = ws.join(".withnav/agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mcpchild.md"),
        "---\nmode: subagent\naccess: full\n---\nUse the tools.",
    )
    .unwrap();
    std::fs::write(
        dir.join("mcpdenied.md"),
        "---\nmode: subagent\naccess: full\ntools:\n  mcp__fake__echo: false\n---\nNo echo.",
    )
    .unwrap();
}

async fn tool_names(provider: &common::sse::CapturingProvider, n: usize) -> Vec<String> {
    wait_for("the provider request", || provider.bodies().len() > n).await;
    let body: Value = serde_json::from_str(&provider.bodies()[n]).unwrap();
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagents_receive_mcp_tools_within_parent_and_definition_limits() {
    for (parent_agent, child_agent, expected) in [
        ("build", "mcpchild", true),
        ("build", "mcpdenied", false),
        ("nomcp", "mcpchild", false),
    ] {
        let env = common::TestEnv::new();
        install_agents(&env.ws);
        std::fs::write(
            env.ws.join(".withnav/agents/nomcp.md"),
            "---\nmode: primary\naccess: full\ntools:\n  mcp__fake__echo: false\n---\nNo echo.",
        )
        .unwrap();
        let provider = spawn_capturing_provider(vec![text_response("child done")]).await;
        let cfg = TestServerConfig::mock(&env.root, &provider.url)
            .chat_tools(128_000)
            .rules_home(&env.home)
            .config();
        with_mcp(&cfg, json!({"command":[common::python(),FIXTURE]}), false);
        let state = AppState::new(cfg).unwrap();
        ready(&state).await;
        let parent = state
            .create_session_as(&env.ws, None, false, Some(parent_agent))
            .await
            .unwrap();
        let child = state
            .create_child_session(parent, child_agent, "inspect")
            .await
            .unwrap();
        let policy: Value = serde_json::from_str(
            &state
                .child_metadata(child)
                .await
                .unwrap()
                .unwrap()
                .policy_snapshot,
        )
        .unwrap();
        assert_eq!(
            policy["mcp__fake__echo"],
            json!(expected),
            "{parent_agent} -> {child_agent}: {policy}"
        );
        let tools = tool_names(&provider, 0).await;
        assert_eq!(
            tools.iter().any(|t| t == "mcp__fake__echo"),
            expected,
            "{parent_agent} -> {child_agent}: {tools:?}"
        );
        if expected {
            assert!(tools.iter().any(|t| t == "mcp__fake__slow"), "{tools:?}");
        }
        state.service().close_all().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nav_run_waits_for_a_server_that_initialises_after_a_delay() {
    let env = common::TestEnv::new();
    let provider = spawn_capturing_provider(vec![text_response("done")]).await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url).rules_home(&env.home);
    cfg.config();
    let models = cfg.models_path();
    let config_dir = env.root.join("config/withnav");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.json"),
        json!({
            "agents":{"build":{"model":"mock/mock-model"}},
            "mcp":{"fake":{"command":[common::python(),FIXTURE],"env":{"INIT_DELAY_SECONDS":"1"},"startup_timeout_ms":5000}}
        })
        .to_string(),
    )
    .unwrap();
    let (root, ws) = (env.root.clone(), env.ws.clone());
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_nav"))
            .args(["run", "--workspace"])
            .arg(&ws)
            .arg("--models")
            .arg(&models)
            .args(["--prompt", "hello"])
            .env("WITHNAV_TEST_KEY", "test-key")
            .env("WITHNAV_CONFIG_DIR", root.join("config/withnav"))
            .env("WITHNAV_DATA_DIR", root.join("data/withnav"))
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let tools = tool_names(&provider, 0).await;
    assert!(tools.iter().any(|t| t == "mcp__fake__echo"), "{tools:?}");
}

fn mcp_note(e: &withnav::bus::event::EventEnvelope) -> bool {
    e.kind == "command.result" && e.payload["name"] == "mcp_tools"
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_keeps_the_tools_it_was_created_with_and_a_later_one_gets_the_parents_current_list()
{
    let env = common::TestEnv::new();
    install_agents(&env.ws);
    let provider = spawn_capturing_provider(vec![
        text_response("early done"),
        text_response("late done"),
    ])
    .await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .chat_tools(128_000)
        .rules_home(&env.home)
        .config();
    with_mcp(
        &cfg,
        json!({"command":[common::python(),FIXTURE],"env":{"INIT_DELAY_SECONDS":"1"}}),
        false,
    );
    let state = AppState::new(cfg).unwrap();
    // The parent starts before the server is ready, and so does the first child.
    let parent = state.create_session(&env.ws, None, false).await.unwrap();
    let sub = state.subscribe(parent, None).await.unwrap();
    let early = state
        .create_child_session(parent, "mcpchild", "inspect")
        .await
        .unwrap();
    let policy = |id| {
        let state = state.clone();
        async move {
            serde_json::from_str::<Value>(
                &state
                    .child_metadata(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .policy_snapshot,
            )
            .unwrap()
        }
    };
    assert!(policy(early).await.get("mcp__fake__echo").is_none());
    // The idle parent takes the server's tools once it is ready, and says so.
    let events = common::server::collect_until(sub, mcp_note).await;
    let note = events.iter().find(|e| mcp_note(e)).unwrap();
    let added = note.payload["data"]["added"].as_array().unwrap();
    assert!(added.iter().any(|t| t == "mcp__fake__echo"), "{note:?}");
    // The first child still holds what it was created with; the next one gets the new list.
    assert!(
        common::server::stored_events(&state, early, "command.result")
            .await
            .iter()
            .all(|e| !mcp_note(e))
    );
    let late = state
        .create_child_session(parent, "mcpchild", "inspect again")
        .await
        .unwrap();
    assert_eq!(policy(late).await["mcp__fake__echo"], json!(true));
    let tools = tool_names(&provider, 0).await;
    assert!(!tools.iter().any(|t| t.starts_with("mcp__")), "{tools:?}");
    state.service().close_all().await;
}

fn tools_file(dir: &Path, names: &[&str]) -> std::path::PathBuf {
    let path = dir.join("tools.json");
    std::fs::write(&path, json!(names).to_string()).unwrap();
    path
}

/// Waits until the manager serves exactly `count` tools of the `fake` server.
async fn serving(state: &AppState, count: usize) {
    wait_for("the server's new tool list", || {
        state
            .service()
            .mcp_statuses()
            .iter()
            .any(|s| s.server_id == "fake" && s.state == "ready" && s.tool_count == count)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_changed_reaches_an_idle_session_only_between_turns() {
    let env = common::TestEnv::new();
    let file = tools_file(&env.root, &["echo"]);
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = common::sse::spawn_gated_provider(
        vec![
            text_response("one"),
            text_response("two"),
            text_response("three"),
        ],
        release.clone(),
    )
    .await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .chat_tools(128_000)
        .rules_home(&env.home)
        .config();
    with_mcp(
        &cfg,
        json!({"command":[common::python(),FIXTURE],"env":{"TOOLS_FILE":file}}),
        false,
    );
    let state = AppState::new(cfg).unwrap();
    ready(&state).await;
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    state
        .send(captain_command(id, "first".into()))
        .await
        .unwrap();
    // The first turn is in flight (its request is held) while the server adds a tool.
    wait_for("the first request", || !provider.bodies().is_empty()).await;
    let sub = state.subscribe(id, None).await.unwrap();
    tools_file(&env.root, &["echo", "alpha"]);
    serving(&state, 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        common::server::stored_events(&state, id, "command.result")
            .await
            .iter()
            .all(|e| !mcp_note(e)),
        "the registry changed during a turn"
    );
    release.notify_one();
    let events = common::server::collect_until(sub, mcp_note).await;
    let note = &events.iter().find(|e| mcp_note(e)).unwrap().payload["data"];
    assert_eq!(note["added"], json!(["mcp__fake__alpha"]));
    assert!(note.get("removed").is_none(), "{note}");
    // The next turn offers it; the one that was running did not.
    state
        .send(captain_command(id, "second".into()))
        .await
        .unwrap();
    let first = tool_names(&provider, 0).await;
    let second = tool_names(&provider, 1).await;
    assert!(!first.iter().any(|t| t == "mcp__fake__alpha"), "{first:?}");
    assert!(second.iter().any(|t| t == "mcp__fake__alpha"), "{second:?}");

    // A tool the server drops disappears from the tools offered the same way.
    let mut finished = 0;
    for _ in 0..200 {
        finished = common::server::stored_events(&state, id, "task.done")
            .await
            .len();
        if finished == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(finished, 2, "the second turn ends");
    let sub = state.subscribe(id, None).await.unwrap();
    tools_file(&env.root, &["alpha"]);
    let events = common::server::collect_until(sub, |e| {
        mcp_note(e) && e.payload["data"]["removed"] == json!(["mcp__fake__echo"])
    })
    .await;
    assert!(events.iter().any(mcp_note));
    state
        .send(captain_command(id, "third".into()))
        .await
        .unwrap();
    let third = tool_names(&provider, 2).await;
    assert!(!third.iter().any(|t| t == "mcp__fake__echo"), "{third:?}");
    assert!(third.iter().any(|t| t == "mcp__fake__alpha"), "{third:?}");
    state.service().close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_session_hears_the_mcp_status_and_the_route_returns_the_same_snapshot() {
    let env = common::TestEnv::new();
    let provider = spawn_capturing_provider(vec![]).await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.home)
        .config();
    // The server answers after a delay, so it turns ready while the session is open.
    with_mcp(
        &cfg,
        json!({"command":[common::python(),FIXTURE],"env":{"INIT_DELAY_SECONDS":"1"},"startup_timeout_ms":5000}),
        false,
    );
    let state = AppState::new(cfg).unwrap();
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();
    let events = common::server::collect_until(sub, |e| {
        e.kind == "mcp.status" && e.payload["servers"][0]["state"] == "ready"
    })
    .await;
    let statuses: Vec<_> = events.iter().filter(|e| e.kind == "mcp.status").collect();
    let last = &statuses.last().unwrap().payload;
    assert_eq!(last["servers"][0]["server_id"], "fake");
    assert_eq!(last["servers"][0]["transport"], "stdio");
    assert!(
        last["servers"][0]["tool_count"].as_u64().unwrap() > 0,
        "{last}"
    );
    // Revisions never go back within one incarnation (a change seen twice may repeat one).
    let revisions: Vec<u64> = statuses
        .iter()
        .map(|e| e.payload["revision"].as_u64().unwrap())
        .collect();
    assert!(revisions.windows(2).all(|w| w[0] <= w[1]), "{revisions:?}");
    // Live-only: never stored, so a resumed conversation does not replay stale servers.
    assert!(
        common::server::stored_events(&state, id, "mcp.status")
            .await
            .is_empty()
    );

    let router = withnav::server::app(state.clone());
    let (status, body) = common::server::request_json(&router, "GET", "/v1/mcp", None).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["incarnation"], last["incarnation"]);
    assert!(
        body["revision"].as_u64() >= last["revision"].as_u64(),
        "{body}"
    );
    assert_eq!(body["servers"][0]["state"], "ready");
    state.service().close_all().await;
}
