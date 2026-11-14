//! Multi-editor language server bridge integration tests against a shared background owner daemon.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};

mod common;
use common::lsp::{frame, read_message};
use common::owner::OwnerGuard;

const HANG_GUARD: Duration = Duration::from_secs(30);

fn setup_workspace(dir: &Path, with_models: bool) {
    let cfg = dir.join("config/withnav");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("config.json"),
        json!({
            "agents": { "build": { "model": "mock/chat" } },
            "layers": { "reflex": "mock/fim" }
        })
        .to_string(),
    )
    .unwrap();

    if with_models {
        std::fs::write(
            cfg.join("models.json"),
            json!({
                "providers": { "mock": { "base_url": "http://127.0.0.1:1/v1", "api_key_env": "WITHNAV_MOCK_KEY" } },
                "models": {
                    "mock/chat": { "provider": "mock", "model": "chat", "context_window": 128000, "supports_tools": true },
                    "mock/fim": { "provider": "mock", "model": "fim", "api": "fim" }
                }
            })
            .to_string(),
        )
        .unwrap();
        withnav::provider::auth::set_key(&cfg, "mock", "mock-secret").unwrap();
    }
}

fn spawn_bridge(root: &Path, workspace: &Path) -> tokio::process::Child {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(cmd.as_std_mut(), root);
    cmd.args(["lsp", "--stdio"])
        .current_dir(workspace)
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("WITHNAV_MOCK_KEY", "mock-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn().expect("spawn bridge")
}

#[tokio::test]
async fn two_editors_share_owner_and_one_closing_does_not_stop_the_other() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data/withnav");
    let _guard = OwnerGuard::new(&data_dir);

    let ws_a = root.path().join("workspace_a");
    let ws_b = root.path().join("workspace_b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    setup_workspace(root.path(), true);

    let mut bridge_a = spawn_bridge(root.path(), &ws_a);
    let mut bridge_b = spawn_bridge(root.path(), &ws_b);

    let init_a = frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "capabilities": {},
            "rootUri": format!("file://{}", ws_a.display())
        }
    }));
    let init_b = frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "capabilities": {},
            "rootUri": format!("file://{}", ws_b.display())
        }
    }));

    bridge_a
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&init_a)
        .await
        .unwrap();
    bridge_b
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&init_b)
        .await
        .unwrap();

    let mut stdout_a = BufReader::new(bridge_a.stdout.take().unwrap());
    let mut stdout_b = BufReader::new(bridge_b.stdout.take().unwrap());

    let info_a = tokio::time::timeout(HANG_GUARD, async {
        loop {
            let msg = read_message(&mut stdout_a).await;
            if msg["id"] == 1 {
                bridge_a
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(&frame(json!({
                        "jsonrpc": "2.0",
                        "method": "initialized",
                        "params": {}
                    })))
                    .await
                    .unwrap();
            }
            if msg["method"] == "withnav/serverInfo" {
                return msg["params"].clone();
            }
        }
    })
    .await
    .unwrap();

    let info_b = tokio::time::timeout(HANG_GUARD, async {
        loop {
            let msg = read_message(&mut stdout_b).await;
            if msg["id"] == 1 {
                bridge_b
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(&frame(json!({
                        "jsonrpc": "2.0",
                        "method": "initialized",
                        "params": {}
                    })))
                    .await
                    .unwrap();
            }
            if msg["method"] == "withnav/serverInfo" {
                return msg["params"].clone();
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(
        info_a["port"], info_b["port"],
        "both editors must attach to the same owner server port"
    );
    assert_eq!(
        info_a["token"], info_b["token"],
        "both editors receive the owner token"
    );

    let client = reqwest::Client::new();
    let port = info_a["port"].as_u64().unwrap();
    let token = info_a["token"].as_str().unwrap();
    let create_res: serde_json::Value = client
        .post(format!("http://127.0.0.1:{port}/v1/sessions"))
        .bearer_auth(token)
        .json(&json!({ "workspace": ws_a.to_str().unwrap(), "editor": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session_id = create_res["session_id"].as_str().unwrap();

    let mut event_stream = client
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/{session_id}/events"
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert!(event_stream.status().is_success());

    let post_res = client
        .post(format!(
            "http://127.0.0.1:{port}/v1/sessions/{session_id}/messages"
        ))
        .bearer_auth(token)
        .json(&json!({
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [{ "type": "text", "text": "live message from second client" }]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        post_res.status().is_success(),
        "second client message: {}",
        post_res.status()
    );

    let event = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = String::new();
        loop {
            let chunk = event_stream
                .chunk()
                .await
                .expect("session stream read failed")
                .expect("session stream closed before posted event");
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buf.find("\n\n") {
                let frame: String = buf.drain(..end + 2).collect();
                let data = frame
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() {
                    continue;
                }
                let value: serde_json::Value = serde_json::from_str(&data).unwrap();
                if value["kind"] == "message.received"
                    && value["payload"].to_string().contains("live message from second client")
                {
                    return value;
                }
            }
        }
    })
    .await
    .expect("posted message event did not arrive before timeout");
    assert_eq!(event["kind"], "message.received");

    let list_res: serde_json::Value = client
        .get(format!("http://127.0.0.1:{port}/v1/sessions"))
        .query(&[("workspace", ws_a.to_str().unwrap())])
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sessions = list_res
        .as_array()
        .or_else(|| list_res["sessions"].as_array())
        .unwrap();
    assert!(
        sessions
            .iter()
            .any(|s| s["id"] == session_id || s["session_id"] == session_id)
    );

    drop(bridge_a.stdin.take());
    let status_a = tokio::time::timeout(Duration::from_secs(5), bridge_a.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status_a.success(), "bridge A exits when stdin closes");

    let ping_b: serde_json::Value = client
        .get(format!("http://127.0.0.1:{port}/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ping_b["status"], "ok");

    drop(bridge_b.stdin.take());
    let _ = bridge_b.wait().await;
}

#[tokio::test]
async fn killing_the_owner_ends_bridge_whose_stdin_stays_open() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data/withnav");
    let guard = OwnerGuard::new(&data_dir);

    let ws = root.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    setup_workspace(root.path(), true);

    let mut bridge = spawn_bridge(root.path(), &ws);
    let init = frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "capabilities": {},
            "rootUri": format!("file://{}", ws.display())
        }
    }));
    bridge
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&init)
        .await
        .unwrap();

    let mut stdout = BufReader::new(bridge.stdout.take().unwrap());
    loop {
        let msg = read_message(&mut stdout).await;
        if msg["id"] == 1 {
            bridge
                .stdin
                .as_mut()
                .unwrap()
                .write_all(&frame(json!({
                    "jsonrpc": "2.0",
                    "method": "initialized",
                    "params": {}
                })))
                .await
                .unwrap();
        }
        if msg["method"] == "withnav/serverInfo" {
            break;
        }
    }

    drop(guard);

    let wait_res = tokio::time::timeout(Duration::from_secs(10), bridge.wait()).await;
    assert!(
        wait_res.is_ok(),
        "bridge must exit when owner disconnects even if stdin is open"
    );
}

#[tokio::test]
async fn failing_preflight_makes_bridge_exit_nonzero_with_stderr_message() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data/withnav");
    let _guard = OwnerGuard::new(&data_dir);

    let ws = root.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    setup_workspace(root.path(), false);

    let mut bridge = spawn_bridge(root.path(), &ws);
    let init = frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "capabilities": {},
            "rootUri": format!("file://{}", ws.display())
        }
    }));
    bridge
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&init)
        .await
        .unwrap();

    let output = tokio::time::timeout(HANG_GUARD, bridge.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !output.status.success(),
        "bridge must exit non-zero when preflight fails"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("default_agent")
            || stderr.contains("models.json")
            || stderr.contains("assigned model"),
        "{stderr}"
    );
}
