//! Process startup admission uses synthetic config and never the driver's credentials.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use navai::agent::instructions::RulesRoots;
use navai::session::preflight::Preflight;
use serde_json::{json, Value};

fn fixture(config: Value) -> tempfile::TempDir {
    let mut config = config;
    if let Some(agents) = config.get_mut("agents").and_then(Value::as_object_mut) {
        for assignment in agents.values_mut() {
            if assignment.is_string() {
                *assignment = json!({"model": assignment.take()});
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config/navai");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.json"), config.to_string()).unwrap();
    std::fs::write(cfg.join("models.json"), json!({
        "providers":{"mock":{"base_url":"http://127.0.0.1:1/v1","api_key_env":"NAVAI_STARTUP_ABSENT_KEY"}},
        "models":{
            "mock/chat":{"provider":"mock","model":"chat","context_window":128000,"supports_tools":true},
            "mock/fim":{"provider":"mock","model":"fim","api":"fim"},
            "mock/no-window":{"provider":"mock","model":"no-window"}
        }
    }).to_string()).unwrap();
    navai::provider::auth::set_key(&cfg, "mock", "synthetic-secret-never-print").unwrap();
    dir
}

async fn captured(dir: &Path) -> Preflight {
    Preflight::capture(&dir.join("config/navai"), None, dir, &RulesRoots::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn default_admission_rejects_unknown_ineligible_and_unusable_assignments() {
    for (config, expected) in [
        (json!({}), "no assigned model"),
        (
            json!({"default_agent":"missing","agents":{"build":"mock/chat"}}),
            "valid agents: build, plan",
        ),
        (
            json!({"default_agent":"explore","agents":{"explore":"mock/chat"}}),
            "not primary",
        ),
        (
            json!({"agents":{"build":"mock/unknown"}}),
            "unknown catalog model",
        ),
        (
            json!({"agents":{"build":"mock/fim"}}),
            "required API 'chat'",
        ),
        (
            json!({"agents":{"build":"mock/no-window"}}),
            "no known context window",
        ),
    ] {
        let dir = fixture(config);
        let err = captured(dir.path())
            .await
            .check(dir.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("default_agent") && err.contains(expected),
            "{err}"
        );
        assert!(!err.contains("synthetic-secret"));
    }
}

#[tokio::test]
async fn missing_or_wrong_api_lower_layers_are_not_startup_failures() {
    for layers in [
        json!({}),
        json!({"reflex":"mock/chat","instinct":"mock/missing","metacognition":"mock/fim"}),
    ] {
        let dir = fixture(json!({"agents":{"build":"mock/chat"},"layers":layers}));
        assert_eq!(
            captured(dir.path()).await.check(dir.path()).await.unwrap(),
            "build"
        );
    }
}

#[tokio::test]
async fn settings_and_models_are_captured_once_and_project_agents_use_actual_scope() {
    let dir = fixture(json!({"default_agent":"local"}));
    let preflight = captured(dir.path()).await;
    assert!(preflight
        .check_provisional(dir.path())
        .await
        .unwrap()
        .is_none());
    let workspace = dir.path().join("project");
    std::fs::create_dir_all(workspace.join(".navai/agents")).unwrap();
    std::fs::write(
        workspace.join(".navai/agents/local.md"),
        "---\nmode: primary\nmodel: mock/chat\n---\nProject role",
    )
    .unwrap();
    std::fs::write(dir.path().join("config/navai/config.json"), "{}").unwrap();
    std::fs::write(dir.path().join("config/navai/models.json"), "{}").unwrap();
    assert_eq!(preflight.check(&workspace).await.unwrap(), "local");
    assert!(preflight
        .check(dir.path())
        .await
        .unwrap_err()
        .to_string()
        .contains("unknown"));
}

#[tokio::test]
async fn missing_or_malformed_auth_is_explicit_and_secret_safe() {
    let dir = fixture(json!({"agents":{"build":"mock/chat"}}));
    let auth = dir.path().join("config/navai/auth.json");
    for raw in [
        None,
        Some(r#"{"mock":{"api_key":true,"synthetic-secret":"bad"}}"#),
    ] {
        if let Some(raw) = raw {
            std::fs::write(&auth, raw).unwrap();
        } else {
            std::fs::remove_file(&auth).unwrap();
        }
        let err = captured(dir.path())
            .await
            .check(dir.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("authentication unavailable") && err.contains("provider 'mock'"),
            "{err}"
        );
        assert!(!err.contains("synthetic-secret"));
    }
}

fn command(dir: &Path, lsp: bool) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(if lsp {
        env!("CARGO_BIN_EXE_navai-lsp")
    } else {
        env!("CARGO_BIN_EXE_navai")
    });
    cmd.current_dir(dir)
        .env("HOME", dir.join("home"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env_remove("NAVAI_STARTUP_ABSENT_KEY")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

#[tokio::test]
async fn overrides_cannot_hide_default_failure_before_run_bind_terminal_or_lsp_readiness() {
    for args in [
        vec!["run", "--model", "mock/chat", "--prompt", "never send"],
        vec!["serve", "--bind", "127.0.0.1:0"],
        vec!["tui", "--model", "mock/chat"],
    ] {
        let dir = fixture(json!({"agents":{"build":"mock/fim"}}));
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            command(dir.path(), false).args(args).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(
            err.contains("default_agent") && err.contains("required API 'chat'"),
            "{err}"
        );
        assert!(!output.stdout.windows(4).any(|s| s == b"\x1b[?1"));
        assert!(!err.contains("synthetic-secret"));
        assert!(!dir.path().join("data/navai/history.sqlite3").exists());
    }
    let dir = fixture(json!({}));
    let output = command(dir.path(), true).output().await.unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("default_agent"));
}

#[tokio::test]
async fn explicit_window_sends_no_provider_requests_and_missing_window_uses_only_discovery() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{
            "id":"chat", "context_length":128000,"supported_reasoning_efforts":["low","high"]
        }]})))
        .mount(&server)
        .await;
    let dir = fixture(json!({"agents":{"build":"mock/chat"}}));
    let models_path = dir.path().join("config/navai/models.json");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&models_path).unwrap()).unwrap();
    models["providers"]["mock"]["base_url"] = json!(format!("{}/v1", server.uri()));
    std::fs::write(&models_path, models.to_string()).unwrap();
    captured(dir.path()).await.check(dir.path()).await.unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    models["providers"]["mock"]["catalog_format"] = json!("omniroute");
    std::fs::write(&models_path, models.to_string()).unwrap();
    captured(dir.path()).await.check(dir.path()).await.unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    models["models"]["mock/chat"]
        .as_object_mut()
        .unwrap()
        .remove("context_window");
    std::fs::write(&models_path, models.to_string()).unwrap();
    captured(dir.path()).await.check(dir.path()).await.unwrap();
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn lsp_actual_initialize_workspace_resolves_project_default_or_exits_before_ready() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    for present in [true, false] {
        let dir = fixture(json!({"default_agent":"local"}));
        let workspace = dir.path().join("project");
        std::fs::create_dir_all(workspace.join(".navai/agents")).unwrap();
        if present {
            std::fs::write(
                workspace.join(".navai/agents/local.md"),
                "---\nmode: primary\nmodel: mock/chat\n---\nProject role",
            )
            .unwrap();
        }
        let body = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "processId":null,"rootUri":format!("file://{}", workspace.display()),"capabilities":{}
        }})
        .to_string();
        let mut child = command(dir.path(), true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
            .await
            .unwrap();
        if present {
            let mut out = child.stdout.take().unwrap();
            let mut bytes = vec![0; 8192];
            let mut received = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let count = out.read(&mut bytes).await.unwrap();
                    assert!(count > 0, "LSP exited before initialize response");
                    received.extend_from_slice(&bytes[..count]);
                    if String::from_utf8_lossy(&received).contains("capabilities") {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            child.start_kill().unwrap();
            child.wait().await.unwrap();
        } else {
            let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            let err = String::from_utf8_lossy(&output.stderr);
            assert!(
                err.contains("default_agent 'local' is unknown")
                    && err.contains("valid agents: build, plan"),
                "{err}"
            );
        }
    }
}

#[tokio::test]
async fn remote_tui_reports_server_diagnostic_without_local_assignment_preflight() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error":"default_agent 'remote' has no usable model"
        })))
        .mount(&server)
        .await;
    let dir = fixture(json!({"default_agent":"local-invalid"}));
    std::fs::write(dir.path().join("config/navai/models.json"), "not json").unwrap();
    let output = command(dir.path(), false)
        .args(["tui", "--url", &server.uri()])
        .output()
        .await
        .unwrap();
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("default_agent 'remote'"), "{err}");
    assert!(
        !err.contains("local-invalid") && !err.contains("startup models"),
        "{err}"
    );
    assert_eq!(output.status.code(), Some(1));
}

#[tokio::test]
async fn global_primary_default_and_invalid_runtime_variant_are_checked() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let dir = fixture(json!({"default_agent":"oracle"}));
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".config/opencode/agents")).unwrap();
    std::fs::write(
        home.join(".config/opencode/agents/oracle.md"),
        "---\nmode: primary\nmodel: mock/chat\n---\nGlobal role",
    )
    .unwrap();
    let snapshot = Preflight::capture(
        &dir.path().join("config/navai"),
        None,
        dir.path(),
        &RulesRoots { home: Some(home) },
    )
    .await
    .unwrap();
    assert_eq!(snapshot.check(dir.path()).await.unwrap(), "oracle");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{
            "id":"chat","context_length":128000,"supported_reasoning_efforts":["low"]
        }]})))
        .mount(&server)
        .await;
    let dir = fixture(json!({"agents":{"build":{"model":"mock/chat","variant":"missing"}}}));
    let models_path = dir.path().join("config/navai/models.json");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&models_path).unwrap()).unwrap();
    models["providers"]["mock"]["base_url"] = json!(format!("{}/v1", server.uri()));
    models["providers"]["mock"]["catalog_format"] = json!("omniroute");
    std::fs::write(models_path, models.to_string()).unwrap();
    let error = captured(dir.path())
        .await
        .check(dir.path())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("default_agent") && error.contains("invalid variant 'missing'"),
        "{error}"
    );
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}
