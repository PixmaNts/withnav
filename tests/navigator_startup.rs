//! The `navai-lsp` binary starts serving without waiting for the provider's
//! model list: discovery belongs to the first remark, not to startup.

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Reads one `Content-Length`-framed JSON-RPC message.
async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> serde_json::Value {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        assert!(r.read_line(&mut line).await.expect("header") > 0, "EOF");
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(n) = line.strip_prefix("Content-Length:") {
            len = n.trim().parse().expect("length");
        }
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await.expect("body");
    serde_json::from_slice(&body).expect("json")
}

// Given a metacognition alias with no window, on a provider whose model list
//       takes far longer than the discovery deadline
// When the binary is started and a client initializes
// Then the initialize answer and the cognition server announcement arrive
//      well before the list does
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_does_not_wait_for_model_discovery() {
    let provider = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": []}))
                .set_delay(Duration::from_secs(20)),
        )
        .mount(&provider)
        .await;

    let root = tempfile::tempdir().expect("tempdir");
    let models = root.path().join("models.json");
    let mut file = std::fs::File::create(&models).expect("models.json");
    write!(
        file,
        "{}",
        json!({
            "providers": { "slow": {
                "base_url": format!("{}/v1", provider.uri()),
                "api_key_env": "NAVAI_TEST_KEY"
            }},
            "models": {
                "metacognition": { "provider": "slow", "model": "m-chat", "supports_tools": true },
                "reflex": { "provider": "slow", "model": "m-fim", "api": "fim" },
                "instinct": { "provider": "slow", "model": "m-s1", "api": "systemone" }
            }
        })
    )
    .expect("write");
    drop(file);

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_navai-lsp"))
        .arg("--models")
        .arg(&models)
        .current_dir(root.path())
        .env("NAVAI_TEST_KEY", "test-key")
        .env("HOME", root.path().join("home"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn navai-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let send = |value: serde_json::Value| {
        let body = value.to_string();
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    };
    let started = Instant::now();
    tokio::io::AsyncWriteExt::write_all(
        &mut stdin,
        send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "processId": null, "rootUri": null, "capabilities": {} }
        }))
        .as_bytes(),
    )
    .await
    .expect("initialize");
    // A client sends `initialized` only once it has the initialize answer.
    let first = tokio::time::timeout(Duration::from_secs(5), read_message(&mut stdout))
        .await
        .expect("the initialize answer arrives while discovery is still pending");
    assert!(first["id"] == 1 && first.get("result").is_some(), "{first}");
    tokio::io::AsyncWriteExt::write_all(
        &mut stdin,
        send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}})).as_bytes(),
    )
    .await
    .expect("initialized");

    let announced = loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), read_message(&mut stdout))
            .await
            .expect("the announcement arrives while discovery is still pending");
        if msg["method"] == "navigator/serverInfo" {
            break msg;
        }
    };
    assert!(
        announced["params"]["port"].as_u64().is_some(),
        "{announced}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "startup waited for discovery: {:?}",
        started.elapsed()
    );
    let _ = child.start_kill();
}
