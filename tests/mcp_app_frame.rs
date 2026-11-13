#![cfg(feature = "test-support")]
//! MCP apps served as documents: `POST /v1/mcp/{server}/app` parks one, `GET /mcp-app/{token}`
//! serves it once under its own Content-Security-Policy, and a real browser runs it in a
//! sandboxed iframe the way the VS Code panel does.
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use withnav::server::{AppState, ServerConfig};

mod common;
use common::server::{TestServerConfig, spawn_server, wait_for};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py");
const TOKEN: &str = "test-bearer-token";

fn with_mcp(cfg: &ServerConfig, env: Value) {
    let mut settings = withnav::settings::load(&cfg.config_dir).unwrap();
    settings.set_dotted(
        "mcp",
        json!({ "fake": {"command": [common::python(), FIXTURE], "env": env} }),
    );
    withnav::settings::save(&cfg.config_dir, &settings).unwrap();
}

async fn app_server(
    root: &Path,
    env: Value,
    token: Option<&str>,
) -> (AppState, SocketAddr, tokio::task::JoinHandle<()>) {
    let mut config = TestServerConfig::mock(root, "http://127.0.0.1:1/v1");
    if let Some(token) = token {
        config = config.bearer_token(token);
    }
    let cfg = config.config();
    with_mcp(&cfg, env);
    let state = AppState::new(
        cfg,
        std::sync::Arc::new(withnav_provider::openai::ClientCache::new()),
    )
    .unwrap();
    wait_for("MCP discovery", || {
        state
            .service()
            .mcp_statuses()
            .iter()
            .any(|s| s.server_id == "fake" && s.state == "ready")
    })
    .await;
    let (addr, task) = spawn_server(state.clone()).await;
    (state, addr, task)
}

async fn mint(client: &reqwest::Client, addr: SocketAddr, body: Value) -> reqwest::Response {
    client
        .post(format!("http://{addr}/v1/mcp/fake/app"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn minted_path(client: &reqwest::Client, addr: SocketAddr, body: Value) -> String {
    let response = mint(client, addr, body).await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    body["path"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_document_is_minted_with_the_bearer_and_loads_once_without_it() {
    let env = common::TestEnv::new();
    let (state, addr, task) = app_server(&env.root, json!({}), Some(TOKEN)).await;
    let client = reqwest::Client::new();
    let post = |body: Value| {
        client
            .post(format!("http://{addr}/v1/mcp/fake/app"))
            .json(&body)
    };

    // Minting is behind the bearer token; a page that only knows the URL cannot start one.
    let denied = post(json!({"uri": "ui://fixture/probe.html"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);

    let path = minted_path(&client, addr, json!({"uri": "ui://fixture/probe.html"})).await;
    assert!(path.starts_with("/mcp-app/") && path.len() > 60, "{path}");
    let url = format!("http://{addr}{path}");

    // A HEAD, a foreign Host and a POST leave the document for the client's own load: only a
    // GET skips the bearer, so the other methods never reach the handler.
    assert_eq!(client.head(&url).send().await.unwrap().status(), 401);
    assert_eq!(client.post(&url).send().await.unwrap().status(), 401);
    let head_with_bearer = client.head(&url).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(
        head_with_bearer.status(),
        405,
        "a HEAD never consumes the token"
    );
    let foreign = client
        .get(&url)
        .header("host", "rebind.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 421);

    let loaded = client.get(&url).send().await.unwrap();
    assert_eq!(loaded.status(), 200);
    let header = |name: &str| loaded.headers()[name].to_str().unwrap().to_string();
    let csp = header("content-security-policy");
    assert!(csp.contains("script-src 'unsafe-inline';"), "{csp}");
    assert!(
        csp.contains("connect-src 'none'"),
        "nothing declared, no network: {csp}"
    );
    assert!(csp.contains("sandbox allow-scripts allow-forms"), "{csp}");
    assert_eq!(header("x-content-type-options"), "nosniff");
    assert_eq!(header("cache-control"), "no-store");
    assert!(header("content-type").starts_with("text/html"));
    assert!(loaded.text().await.unwrap().contains("ui/initialize"));

    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        404,
        "single use"
    );
    assert_eq!(
        client
            .get(format!("http://{addr}/mcp-app/unknown"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    // Everything else stays behind the bearer token.
    assert_eq!(
        client
            .get(format!("http://{addr}/v1/mcp"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    state.service().close_all().await;
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_the_resource_declares_is_the_only_network_it_gets() {
    let env = common::TestEnv::new();
    let (state, addr, task) = app_server(
        &env.root,
        json!({"APP_CONNECT": "https://api.example.com https://x.example.com;script-src-* ws://live.test:9"}),
        Some(TOKEN),
    )
    .await;
    let client = reqwest::Client::new();
    let path = minted_path(&client, addr, json!({"uri": "ui://fixture/probe.html"})).await;
    let loaded = client
        .get(format!("http://{addr}{path}"))
        .send()
        .await
        .unwrap();
    let csp = loaded.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        csp.contains("connect-src https://api.example.com ws://live.test:9;"),
        "{csp}"
    );
    assert!(!csp.contains("script-src-"), "{csp}");

    // Inline HTML has no declaration at all.
    let path = minted_path(&client, addr, json!({"html": "<p>inline</p>"})).await;
    let loaded = client
        .get(format!("http://{addr}{path}"))
        .send()
        .await
        .unwrap();
    let csp = loaded.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("connect-src 'none'"), "{csp}");
    assert_eq!(loaded.text().await.unwrap(), "<p>inline</p>");
    state.service().close_all().await;
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minting_refuses_what_is_not_an_app_document() {
    let env = common::TestEnv::new();
    let (state, addr, task) = app_server(&env.root, json!({}), Some(TOKEN)).await;
    let client = reqwest::Client::new();
    for body in [
        json!({"uri": "ui://fixture/app.html", "html": "<p>"}),
        json!({}),
        json!({"html": "x".repeat(512 * 1024 + 1)}),
    ] {
        let response = mint(&client, addr, body.clone()).await;
        assert_eq!(
            response.status(),
            400,
            "{}",
            body.to_string().chars().take(60).collect::<String>()
        );
    }

    let invalid_uri = mint(&client, addr, json!({"uri": "file:///etc/passwd"})).await;
    assert_eq!(invalid_uri.status(), 400);
    let body: Value = invalid_uri.json().await.unwrap();
    assert_eq!(body["reason"], "mcp_invalid_request");

    let unknown = client
        .post(format!("http://{addr}/v1/mcp/nobody/app"))
        .bearer_auth(TOKEN)
        .json(&json!({"uri": "ui://fixture/app.html"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let body: Value = unknown.json().await.unwrap();
    assert_eq!(body["reason"], "mcp_unknown_server");

    let invalid_mime = mint(&client, addr, json!({"uri": "ui://fixture/plain.html"})).await;
    assert_eq!(invalid_mime.status(), 502);
    let body: Value = invalid_mime.json().await.unwrap();
    assert_eq!(body["reason"], "mcp_invalid_response");

    state.service().close_all().await;
    let unavail = mint(&client, addr, json!({"uri": "ui://fixture/app.html"})).await;
    assert_eq!(unavail.status(), 503);
    let body: Value = unavail.json().await.unwrap();
    assert_eq!(body["reason"], "mcp_unavailable");

    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_document_is_decoded_and_redacted_and_waiting_ones_are_capped() {
    let env = common::TestEnv::new();
    let (state, addr, task) = app_server(
        &env.root,
        json!({"SECRET": "correct-horse-battery"}),
        Some(TOKEN),
    )
    .await;
    let client = reqwest::Client::new();
    let path = minted_path(&client, addr, json!({"uri": "ui://fixture/blob.html"})).await;
    let body = client
        .get(format!("http://{addr}{path}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.starts_with("<p>") && !body.contains("correct-horse-battery"),
        "{body}"
    );

    let unpadded_path = minted_path(
        &client,
        addr,
        json!({"uri": "ui://fixture/unpadded-blob.html"}),
    )
    .await;
    let unpadded_body = client
        .get(format!("http://{addr}{unpadded_path}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        unpadded_body.starts_with("<p>") && !unpadded_body.contains("correct-horse-battery"),
        "{unpadded_body}"
    );

    let mut statuses = vec![];
    for _ in 0..40 {
        statuses.push(
            mint(&client, addr, json!({"html": "<p>"}))
                .await
                .status()
                .as_u16(),
        );
    }
    assert!(
        statuses.contains(&503),
        "the number of waiting documents is bounded"
    );
    state.service().close_all().await;
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_without_a_bearer_token_never_mints() {
    let env = common::TestEnv::new();
    let (state, addr, task) = app_server(&env.root, json!({}), None).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/mcp/fake/app"))
        .json(&json!({"html": "<p>"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    state.service().close_all().await;
    task.abort();
}

/// A browser to run the sandboxed document in: `WITHNAV_TEST_BROWSER`, else the first of the
/// usual names on `PATH`. The test is skipped, loudly, without one.
fn browser() -> Option<String> {
    if let Ok(path) = std::env::var("WITHNAV_TEST_BROWSER") {
        return Some(path);
    }
    let path = std::env::var_os("PATH")?;
    [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
    ]
    .iter()
    .find(|name| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
    .map(|name| (*name).to_string())
}

/// Records the request targets that reach a loopback port, and answers each with an empty 200.
async fn recording_listener() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = vec![0u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let target = request.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(target);
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    (port, seen)
}

/// The panel's side of the bridge, reduced to what the probe needs: answer `ui/initialize`,
/// and send the app's report to `collector`, since a headless run cannot be read back reliably
/// from the page. Each step also reaches `collector` as `/stage/<name>`, so a run that never
/// reports says how far it got. The frame is created only once the bridge listens, so its
/// `ui/initialize` cannot arrive before anyone answers it.
fn host_page(frame_url: &str, collector: u16) -> String {
    format!(
        r#"<!doctype html><body>
<script>
const collector = "http://127.0.0.1:{collector}";
const stage = (name) => fetch(collector + "/stage/" + name, {{mode: "no-cors"}}).catch(() => {{}});
stage("host");
const frame = document.createElement("iframe");
frame.setAttribute("sandbox", "allow-scripts allow-forms");
frame.addEventListener("load", () => stage("frame-load"));
window.addEventListener("message", (event) => {{
  if (event.source !== frame.contentWindow) return;
  const data = event.data;
  if (data.method === "ui/initialize") {{
    stage("initialize");
    frame.contentWindow.postMessage({{jsonrpc: "2.0", id: data.id, result: {{protocolVersion: "2025-11-25"}}}}, "*");
  }} else if (data.method === "probe/result") {{
    fetch(collector + "/report?" + encodeURIComponent(JSON.stringify(data.params)), {{mode: "no-cors"}});
  }}
}});
frame.src = "{frame_url}";
document.body.appendChild(frame);
</script></body>"#
    )
}

/// The last lines of the browser's log, each one bounded, with the document token and the
/// bearer token replaced: a failure message must not hand either to whoever reads the CI log.
fn log_tail(path: &Path, secrets: &[&str]) -> String {
    let text = std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_else(|error| format!("<no browser log: {error}>"));
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(40)..]
        .iter()
        .map(|line| {
            let mut line = (*line).to_string();
            for secret in secrets {
                line = line.replace(secret, "<redacted>");
            }
            line.chars().take(400).collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_browser_runs_the_app_in_a_sandbox_that_reaches_nothing_it_did_not_declare() {
    let Some(browser) = browser() else {
        eprintln!("skipped: no Chromium or Chrome on PATH (set WITHNAV_TEST_BROWSER)");
        return;
    };
    let (declared_port, declared_hits) = recording_listener().await;
    let (undeclared_port, undeclared_hits) = recording_listener().await;
    let (collector_port, reports) = recording_listener().await;
    let env = common::TestEnv::new();
    let (state, addr, task) = app_server(
        &env.root,
        json!({"APP_CONNECT_URL": format!("http://127.0.0.1:{declared_port}")}),
        Some(TOKEN),
    )
    .await;
    let client = reqwest::Client::new();
    let path = minted_path(&client, addr, json!({"uri": "ui://fixture/probe.html"})).await;
    let frame_url = format!("http://{addr}{path}#{declared_port},{undeclared_port}");
    let profile = tempfile::tempdir().unwrap();
    let host_file = profile.path().join("host.html");
    std::fs::write(&host_file, host_page(&frame_url, collector_port)).unwrap();
    let log_path = profile.path().join("browser.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut browser = tokio::process::Command::new(&browser)
        .args([
            "--headless",
            "--no-sandbox",
            "--disable-gpu",
            "--no-first-run",
            "--no-proxy-server",
            "--enable-logging=stderr",
        ])
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .arg(format!("file://{}", host_file.display()))
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let has_report = || {
        reports
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.starts_with("/report?"))
    };
    // Two bounds, so a slow browser and a stuck app stay apart. A cold Chromium on the hosted
    // Linux runner first ran the host page 11.7 s after it was spawned (2026-11-10), and printed
    // nothing for 16.5 s in a run that then missed a single 20 s bound; the app itself went from
    // the host page to its report in 0.2 s. Startup gets 40 s, which keeps the frame's load
    // inside the 60 s a minted document waits; once the host page runs, the app keeps the 20 s
    // it always had.
    let started = tokio::time::Instant::now();
    let startup_deadline = started + std::time::Duration::from_secs(40);
    let app_bound = std::time::Duration::from_secs(20);
    let mut host_ran: Option<tokio::time::Instant> = None;
    // When each step reached the collector, in milliseconds after the browser was started.
    let mut timeline: Vec<String> = Vec::new();
    let record = |timeline: &mut Vec<String>| {
        let seen = reports.lock().unwrap();
        for target in &seen[timeline.len()..] {
            let step = target.split('?').next().unwrap_or("");
            timeline.push(format!("{} ms {step}", started.elapsed().as_millis()));
        }
    };
    while !has_report() {
        record(&mut timeline);
        if host_ran.is_none() && timeline.iter().any(|step| step.ends_with(" /stage/host")) {
            host_ran = Some(tokio::time::Instant::now());
        }
        let deadline = host_ran.map_or(startup_deadline, |at| at + app_bound);
        let exited = browser.try_wait().unwrap();
        if exited.is_none() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            continue;
        }
        let failure = match (exited, host_ran) {
            (Some(status), _) => format!("the browser exited before the app's report ({status})"),
            (None, None) => "timed out waiting for the browser to run the host page".to_string(),
            (None, Some(_)) => "timed out waiting for the app's report".to_string(),
        };
        let _ = browser.kill().await;
        // The document is single use and expires: a 404 now means taken or expired.
        let document = client
            .get(format!("http://{addr}{path}"))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .map(|response| match response.status().as_u16() {
                404 => "already taken or expired".to_string(),
                200 => "never taken".to_string(),
                other => format!("status {other}"),
            })
            .unwrap_or_else(|error| format!("unknown: {}", error.without_url()));
        record(&mut timeline);
        let token = path.trim_start_matches("/mcp-app/");
        panic!(
            "{failure} after {} ms\nstages reached: {timeline:?}\napp document: {document}\nbrowser log:\n{}",
            started.elapsed().as_millis(),
            log_tail(&log_path, &[token, TOKEN])
        );
    }
    let _ = browser.kill().await;
    let target = reports
        .lock()
        .unwrap()
        .iter()
        .find(|t| t.starts_with("/report?"))
        .cloned()
        .unwrap();
    let encoded = target.trim_start_matches("/report?");
    let decoded: String = {
        let bytes = encoded.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                out.push(u8::from_str_radix(&encoded[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    };
    let report: Value = serde_json::from_str(&decoded).unwrap();

    assert_eq!(
        report["initialized"], "2025-11-25",
        "ui/initialize reached the bridge: {report}"
    );
    assert_eq!(report["parentDocument"], "blocked", "{report}");
    assert_eq!(report["undeclared"], "blocked", "{report}");
    assert_eq!(report["declared"], "reached", "{report}");
    assert!(
        undeclared_hits.lock().unwrap().is_empty(),
        "no connection to an undeclared origin"
    );
    assert!(
        !declared_hits.lock().unwrap().is_empty(),
        "the declared origin was reachable"
    );
    state.service().close_all().await;
    task.abort();
}
