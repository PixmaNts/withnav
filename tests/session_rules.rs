//! Local instructions through the session service: a scripted provider
//! captures the request, so the order of the system messages is checked on
//! the wire. The home directory is a temporary one in every test.

use std::path::{Path, PathBuf};

use serde_json::Value;

use withnav::bus::command::{Command, ContentBlock, DeliveryMode};
use withnav::server::AppState;

mod common;
use common::server::{collect_until, TestServerConfig};
use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_chunk, CapturingProvider};

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    ws: PathBuf,
}

/// A temp home and a Git workspace holding its own `AGENTS.md`.
fn env() -> Env {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonical");
    let home = root.join("home");
    let ws = root.join("ws");
    std::fs::create_dir_all(home.join(".agents")).unwrap();
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::write(home.join(".agents/AGENTS.md"), "global rule").unwrap();
    std::fs::write(ws.join("AGENTS.md"), "project rule").unwrap();
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

async fn provider(answers: &[&str]) -> CapturingProvider {
    spawn_capturing_provider(
        answers
            .iter()
            .map(|a| http_sse(&sse_body(&[&text_chunk(a), &finish()])))
            .collect(),
    )
    .await
}

fn finish() -> String {
    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_string()
}

/// Sends `text` and waits for the turn to end.
async fn converse(state: &AppState, id: ulid::Ulid, text: &str) {
    let sub = state.subscribe(id, None).await.expect("subscribe");
    state
        .send(Command {
            command_id: ulid::Ulid::generate(),
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
    let events = collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    assert!(
        events.iter().any(|e| e.kind == "turn.end"),
        "the turn ended"
    );
}

/// `(role, content)` of the messages of the `n`th captured request.
fn messages(provider: &CapturingProvider, n: usize) -> Vec<(String, String)> {
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

fn rules_label(path: &Path) -> String {
    format!("Instructions from {}:", path.display())
}

// Given a home file and a project file
// When a session is created and a message is sent
// Then the request is: contract, ONE rules message (global then project,
//      each labelled), then the user message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rules_message_follows_the_contract_and_precedes_history() {
    let env = env();
    let provider = provider(&["ok"]).await;
    let state = state(&env, &provider, "default", false);
    let id = state
        .create_session(&env.ws, None, false)
        .await
        .expect("create");
    converse(&state, id, "hello").await;

    let sent = messages(&provider, 0);
    let roles: Vec<&str> = sent.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(roles, ["system", "system", "system", "user"], "{sent:?}");
    assert!(sent[0]
        .1
        .starts_with("You are a headless Rust coding agent"));
    let rules = &sent[2].1;
    let global = rules
        .find(&rules_label(&env.home.join(".agents/AGENTS.md")))
        .unwrap();
    let project = rules.find(&rules_label(&env.ws.join("AGENTS.md"))).unwrap();
    assert!(global < project, "{rules}");
    assert!(rules.contains("global rule") && rules.contains("project rule"));
    assert_eq!(sent[3].1, "hello");
}

// Given no rules file at all
// When a message is sent
// Then there is no rules message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_rules_the_request_has_no_extra_message() {
    let env = env();
    std::fs::remove_file(env.home.join(".agents/AGENTS.md")).unwrap();
    std::fs::remove_file(env.ws.join("AGENTS.md")).unwrap();
    let provider = provider(&["ok"]).await;
    let state = state(&env, &provider, "default", false);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    converse(&state, id, "hello").await;
    let roles: Vec<String> = messages(&provider, 0).into_iter().map(|(r, _)| r).collect();
    assert_eq!(roles, ["system", "system", "user"]);
}

// Given a navigator session
// When a message is sent
// Then the order is: contract, the navigator role, the rules, the user
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_navigator_role_stays_before_the_rules() {
    let env = env();
    let provider = provider(&["ok"]).await;
    let state = state(&env, &provider, "cognition", true);
    let id = state
        .create_session(&env.ws, None, true)
        .await
        .expect("create");
    converse(&state, id, "hello").await;

    let sent = messages(&provider, 0);
    let roles: Vec<&str> = sent.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(roles, ["system", "system", "system", "user"], "{sent:?}");
    assert!(sent[1].1.starts_with("You are Nav · The Navigator"));
    assert!(sent[1].1.ends_with(withnav::navigator::COGNITION_PROMPT));
    assert!(sent[2].1.contains("project rule"));
}

// Given a conversation closed and resumed in a fresh service after the rules
//       file changed
// When a message is sent
// Then the resumed request carries the CURRENT rules right after the
//      contract, and the earlier turn after them; the rules are not in the
//      stored transcript
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_reloads_the_rules_once() {
    let env = env();
    let provider = provider(&["first", "second", "third"]).await;
    let first = state(&env, &provider, "default", false);
    let id = first.create_session(&env.ws, None, false).await.unwrap();
    converse(&first, id, "one").await;

    // The snapshot holds for the life of the session: no rediscovery.
    std::fs::write(env.ws.join("AGENTS.md"), "edited while running").unwrap();
    converse(&first, id, "two").await;
    assert!(messages(&provider, 1)[2].1.contains("project rule"));
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
    assert_eq!(sent[0].0, "system");
    assert!(sent[2].1.contains("edited while running"), "{sent:?}");
    assert!(!sent[2].1.contains("project rule"));
    assert_eq!(sent[3], ("user".to_string(), "one".to_string()));
    let rules_count = sent
        .iter()
        .filter(|(_, c)| c.contains("Instructions from"))
        .count();
    assert_eq!(rules_count, 1, "{sent:?}");
}

// Given the `nav run` binary with a temporary HOME holding an AGENTS.md
// When it runs a one-shot task against a scripted provider
// Then the request it sends carries that file as the rules message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withnav_run_sends_the_rules_too() {
    let env = env();
    let provider = provider(&["done"]).await;
    let config = TestServerConfig::mock(&env.root, &provider.url);
    config.config();
    std::fs::create_dir_all(env.root.join("config/withnav")).unwrap();
    std::fs::write(
        env.root.join("config/withnav/config.json"),
        r#"{"agents":{"build":{"model":"mock/mock-model"}}}"#,
    )
    .unwrap();
    let models = env.root.join("models.json");
    let (ws, root) = (env.ws.clone(), env.root.clone());
    let out = tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_nav"));
        common::isolate(&mut command, &root);
        command
            .args(["run", "--workspace"])
            .arg(&ws)
            .arg("--models")
            .arg(&models)
            .args(["--prompt", "go"])
            .env("WITHNAV_TEST_KEY", "test-key")
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .output()
            .expect("run the binary")
    })
    .await
    .expect("join");
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    let sent = messages(&provider, 0);
    let roles: Vec<&str> = sent.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(roles, ["system", "system", "system", "user"], "{sent:?}");
    assert!(sent[1]
        .1
        .contains(&std::env::temp_dir().join("withnav").display().to_string()));
    assert!(sent[2].1.contains("global rule") && sent[2].1.contains("project rule"));
}
