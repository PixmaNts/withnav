//! `nav lsp` publishes its session server for the terminal client of the same workspace, in a
//! private runtime directory, and unpublishes it when the editor ends the session.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use common::lsp::{frame, read_message};
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};

mod common;

/// A hang guard for reads from the child process, not a performance bound.
const HANG_GUARD: Duration = Duration::from_secs(30);

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn published(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(listing) => listing
            .map(|item| item.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

async fn create_session((url, token): &(String, String), workspace: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/sessions"))
        .bearer_auth(token)
        .json(&json!({ "workspace": workspace }))
        .send()
        .await
        .expect("create");
    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("json");
    assert!(status.is_success(), "{status}: {body}");
    body["session_id"].as_str().expect("session_id").to_owned()
}

async fn list_sessions((url, token): &(String, String), workspace: &str) -> Vec<String> {
    let response = reqwest::Client::new()
        .get(format!("{url}/v1/sessions"))
        .query(&[("workspace", workspace)])
        .bearer_auth(token)
        .send()
        .await
        .expect("list");
    assert!(response.status().is_success(), "{}", response.status());
    let body: serde_json::Value = response.json().await.expect("json");
    body["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        // A live entry is open in this very server, not merely found in the shared database.
        .filter(|entry| entry["live"] == true)
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect()
}

// Given an editor that starts `nav lsp` in a workspace, with a private runtime directory
// When the LSP announces its chat server
// Then an owner-only discovery file points at that same server, the terminal client's lookup
//      finds it, a conversation started on either side is listed on the other, and the file is
//      gone once the editor shuts the LSP down
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lsp_publishes_its_server_until_it_exits() {
    let root = tempfile::tempdir().expect("tempdir");
    let base = withnav::server::discovery::canonical_workspace(root.path()).unwrap();
    let workspace = base.join("project");
    let runtime = base.join("runtime");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();

    let models = base.join("models.json");
    let mut file = std::fs::File::create(&models).expect("models.json");
    write!(
        file,
        "{}",
        json!({
            "providers": { "p": { "base_url": "http://127.0.0.1:1/v1", "api_key_env": "WITHNAV_TEST_KEY" }},
            "models": {
                "p/main": { "provider": "p", "model": "main", "context_window": 128000, "supports_tools": true }
            }
        })
    )
    .expect("write");
    drop(file);
    let config = base.join("config/withnav");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.json"),
        json!({"agents": {"build": {"model": "p/main"}}}).to_string(),
    )
    .unwrap();

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(command.as_std_mut(), &base);
    let mut child = command
        .args(["lsp", "--stdio"])
        .arg("--models")
        .arg(&models)
        .current_dir(&workspace)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("WITHNAV_TEST_KEY", "test-key")
        .env("HOME", base.join("home"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn nav lsp");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    stdin
        .write_all(&frame(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "processId": null, "rootUri": null, "capabilities": {} }
        })))
        .await
        .unwrap();
    let first = tokio::time::timeout(HANG_GUARD, read_message(&mut stdout))
        .await
        .expect("initialize answer");
    assert!(first["id"] == 1 && first.get("result").is_some(), "{first}");
    stdin
        .write_all(&frame(
            json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
        ))
        .await
        .unwrap();
    let announced = loop {
        let msg = tokio::time::timeout(HANG_GUARD, read_message(&mut stdout))
            .await
            .expect("serverInfo");
        if msg["method"] == "withnav/serverInfo" {
            break msg;
        }
    };
    let port = announced["params"]["port"].as_u64().expect("port");
    let token = announced["params"]["token"]
        .as_str()
        .expect("token")
        .to_owned();

    let dir = runtime.join(withnav::server::discovery::DIR_NAME);
    let files = published(&dir);
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&files[0]), 0o600);

    let found = withnav::server::discovery::find(&dir, &workspace)
        .await
        .expect("the terminal client finds the editor's server");
    assert_eq!(u64::from(found.port), port);
    assert_eq!(found.token, token);

    // A conversation started by the editor is listed for the terminal client, and the reverse.
    let editor = (format!("http://127.0.0.1:{port}"), token.clone());
    let terminal = (found.url(), found.token.clone());
    let ws = workspace.to_str().unwrap();
    let from_editor = create_session(&editor, ws).await;
    assert!(list_sessions(&terminal, ws).await.contains(&from_editor));
    let from_terminal = create_session(&terminal, ws).await;
    assert!(list_sessions(&editor, ws).await.contains(&from_terminal));

    stdin
        .write_all(&frame(
            json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
        ))
        .await
        .unwrap();
    loop {
        let msg = tokio::time::timeout(HANG_GUARD, read_message(&mut stdout))
            .await
            .expect("shutdown answer");
        if msg["id"] == 2 {
            break;
        }
    }
    stdin
        .write_all(&frame(json!({"jsonrpc": "2.0", "method": "exit"})))
        .await
        .unwrap();
    drop(stdin);
    let status = tokio::time::timeout(HANG_GUARD, child.wait())
        .await
        .expect("nav lsp exits")
        .expect("wait");
    assert!(status.success(), "{status}");
    assert!(
        published(&dir).is_empty(),
        "the discovery file outlived the LSP"
    );
}
