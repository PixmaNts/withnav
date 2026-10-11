mod common;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use withnav::app::{
    doctor::{inspect, Report},
    DoctorArgs,
};

const SECRET: &str = "secret-doctor-auth-sentinel";

#[derive(Clone)]
struct Fake {
    calls: Arc<AtomicUsize>,
    status: u16,
    invalid: bool,
    delay: Duration,
}
async fn systemone(State(fake): State<Fake>, Json(body): Json<Value>) -> axum::response::Response {
    fake.calls.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(fake.delay).await;
    if fake.status != 200 {
        return (
            StatusCode::from_u16(fake.status).unwrap(),
            format!("{SECRET} https://user:password@host/key"),
        )
            .into_response();
    }
    let mut answers = serde_json::Map::new();
    if !fake.invalid {
        for (name, question) in body["questions"].as_object().unwrap() {
            answers.insert(name.clone(), if question["type"] == "noul" {json!({"type":"noul","noul":0.5})}
                else {json!({"type":"choice","choice":question["criteria"].as_object().unwrap().keys().next().unwrap(),"probabilities":{question["criteria"].as_object().unwrap().keys().next().unwrap():1.0},"confidence":0.9})});
        }
    }
    Json(json!({"answers":answers})).into_response()
}
async fn fim(State(fake): State<Fake>) -> axum::response::Response {
    fake.calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({"choices":[{"text":if fake.invalid {""} else {"println!(\"hello\");"}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})).into_response()
}
async fn chat(State(fake): State<Fake>, Json(body): Json<Value>) -> axum::response::Response {
    fake.calls.fetch_add(1, Ordering::SeqCst);
    assert!(body.get("tools").is_none());
    assert_eq!(body["reasoning_effort"], "low");
    let content = if fake.invalid { "" } else { "OK" };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        common::navigator::sse_stream(vec![common::navigator::sse_data(
            &common::navigator::delta_text(content),
        )]),
    )
        .into_response()
}
async fn metadata(State(fake): State<Fake>) -> Json<Value> {
    fake.calls.fetch_add(1, Ordering::SeqCst);
    Json(
        json!({"data":[{"id":"chat","context_length":128000,"capabilities":{"effort_tiers":["low","high"]}}]}),
    )
}
async fn server(
    status: u16,
    invalid: bool,
    delay: Duration,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/systemone", post(systemone))
        .route("/v1/fim/completions", post(fim))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(metadata))
        .with_state(Fake {
            calls: calls.clone(),
            status,
            invalid,
            delay,
        });
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, calls, task)
}
fn fixture(root: &Path, base: &str) -> (std::path::PathBuf, DoctorArgs) {
    let config = root.join("withnav");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(workspace.join(".withnav")).unwrap();
    std::fs::write(
        workspace.join(".withnav/intent.md"),
        "# Intent\nSynthetic test",
    )
    .unwrap();
    withnav::provider::auth::set_key(&config, "fake", SECRET).unwrap();
    std::fs::write(config.join("models.json"), json!({"providers":{"fake":{"base_url":base,"api_key_env":"DOCTOR_TEST_UNSET"}},"models":{
        "fake/fim":{"provider":"fake","model":"fim","api":"fim"},
        "fake/one":{"provider":"fake","model":"one","api":"systemone"},
        "fake/chat":{"provider":"fake","model":"chat","context_window":128000,"default_variant":"high","variants":{"low":{"reasoning_effort":"low"},"high":{"reasoning_effort":"high"}}}
    }}).to_string()).unwrap();
    std::fs::write(config.join("config.json"), json!({"layers":{"reflex":"fake/fim","instinct":"fake/one","metacognition":"fake/chat"},"agents":{"build":{"model":"fake/chat"},"plan":{"model":"fake/chat"}}}).to_string()).unwrap();
    (
        config,
        DoctorArgs {
            offline: false,
            json: false,
            workspace,
            layer: None,
        },
    )
}
fn check<'a>(report: &'a Report, id: &str) -> &'a withnav::app::doctor::Check {
    report
        .checks
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("missing {id}: {}", report.human()))
}
fn snapshot(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut entries = vec![];
    for entry in std::fs::read_dir(root).unwrap().flatten() {
        if entry.path().is_dir() {
            entries.extend(snapshot(&entry.path()));
        } else {
            entries.push((entry.path(), std::fs::read(entry.path()).unwrap()));
        }
    }
    entries.sort();
    entries
}

#[tokio::test]
async fn successful_live_layers_and_offline_zero_calls_read_only() {
    let (base, calls, task) = server(200, false, Duration::ZERO).await;
    let root = tempfile::tempdir().unwrap();
    let (config, mut args) = fixture(root.path(), &base);
    let before = snapshot(root.path());
    let report = inspect(&args, &config, None, Some(root.path().join("data"))).await;
    for id in [
        "live.reflex.plain",
        "live.reflex.context",
        "live.instinct.editor",
        "live.instinct.role_guard",
        "live.instinct.permission",
        "live.metacognition.chat",
    ] {
        assert_eq!(
            check(&report, id).status,
            withnav::app::doctor::Status::Ok,
            "{id}: {}",
            report.human()
        );
    }
    assert_eq!(report.exit_code(), 0, "{}", report.human());
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert_eq!(snapshot(root.path()), before);
    calls.store(0, Ordering::SeqCst);
    args.offline = true;
    let report = inspect(&args, &config, None, Some(root.path().join("data"))).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(report.exit_code(), 0, "{}", report.human());
    assert_eq!(snapshot(root.path()), before);
    assert!(check(&report, "live.instinct").value.contains("skipped"));
    task.abort();
}

#[tokio::test]
async fn invalid_missing_answers_and_http_errors_are_sanitized() {
    for (status, invalid, category) in [
        (200, true, "invalid_answer"),
        (401, false, "http_rejected"),
        (503, false, "http_server"),
    ] {
        let (base, _, task) = server(status, invalid, Duration::ZERO).await;
        let root = tempfile::tempdir().unwrap();
        let (config, mut args) = fixture(root.path(), &base);
        args.layer = Some(withnav::app::doctor::DoctorLayer::Instinct);
        let report = inspect(&args, &config, None, None).await;
        assert_eq!(report.exit_code(), 1);
        for id in [
            "live.instinct.editor",
            "live.instinct.role_guard",
            "live.instinct.permission",
        ] {
            assert_eq!(check(&report, id).category.as_deref(), Some(category));
        }
        let json = serde_json::to_string(&report).unwrap();
        for output in [report.human(), json] {
            assert!(!output.contains(SECRET));
            assert!(!output.contains("password@"));
            assert!(!output.contains(&base));
        }
        task.abort();
    }
}

#[tokio::test]
async fn unreachable_and_systemone_two_second_deadline() {
    for slow in [false, true] {
        let (base, _, task) = server(200, false, Duration::from_secs(3)).await;
        if !slow {
            task.abort();
            tokio::task::yield_now().await;
        }
        let root = tempfile::tempdir().unwrap();
        let (config, mut args) = fixture(root.path(), &base);
        args.layer = Some(withnav::app::doctor::DoctorLayer::Instinct);
        let report = inspect(&args, &config, None, None).await;
        assert_eq!(
            check(&report, "live.instinct.editor").category.as_deref(),
            Some(if slow { "timeout" } else { "unreachable" })
        );
        if slow {
            assert!(
                (1900..2900).contains(&check(&report, "live.instinct.editor").elapsed_ms.unwrap())
            );
        }
        task.abort();
    }
}

#[tokio::test]
async fn fixtures_unknown_keys_aliases_permissions_defaults_flows_and_variants() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let (config, mut args) = fixture(root.path(), "http://127.0.0.1:1/v1");
    args.offline = true;
    std::fs::create_dir_all(args.workspace.join(".navai")).unwrap();
    std::fs::create_dir_all(config.join("agents")).unwrap();
    std::fs::write(config.join("agents/explore.md"), "---\ninvalid: [\n---\n").unwrap();
    std::fs::write(config.join("config.json"), json!({"unknown":SECRET,"layers":{"instinct":"fake/chat","metacognition":{"model":"fake/chat","variant":"off"}},"agents":{"build":{"model":"fake/chat"},"plan":{"model":"fake/chat","variant":"missing"},"ghost":{"model":"fake/chat"}},"default_agent":"build","compaction":{"writer":"session","chunk_tokens":1024}}).to_string()).unwrap();
    std::fs::create_dir_all(args.workspace.join(".withnav/flows")).unwrap();
    std::fs::write(
        args.workspace.join(".withnav/flows/broken.toml"),
        "this must remain unread until trusted",
    )
    .unwrap();
    let before = snapshot(root.path());
    let report = inspect(&args, &config, None, None).await;
    assert_eq!(report.exit_code(), 0, "{}", report.human());
    assert!(check(&report, "assignment.layer.instinct")
        .value
        .contains("must use"));
    assert!(check(&report, "assignment.agent.plan")
        .value
        .contains("invalid variant"));
    assert!(check(&report, "assignment.agent.vigie")
        .value
        .contains("inherits parent"));
    assert!(check(&report, "assignment.compaction")
        .value
        .contains("writer=session"));
    assert!(check(&report, "variant.metacognition")
        .value
        .contains("off"));
    assert!(report
        .checks
        .iter()
        .find(|c| c.id.contains("legacy.agent.") && c.id.ends_with(".explore"))
        .unwrap()
        .hint
        .as_ref()
        .unwrap()
        .contains("vigie"));
    assert!(check(&report, "agent.configured.ghost")
        .value
        .contains("unknown"));
    assert!(!serde_json::to_string(&report).unwrap().contains(SECRET));
    assert_eq!(snapshot(root.path()), before);
    let canonical = args.workspace.canonicalize().unwrap();
    std::fs::write(config.join("flows-trust.json"), json!({"version":1,"workspaces":{canonical.display().to_string():{"trusted":true,"granted_at":"2026-01-01T00:00:00Z"}}}).to_string()).unwrap();
    let report = inspect(&args, &config, None, None).await;
    assert_eq!(
        check(&report, "flows.workspace.broken.toml").status,
        withnav::app::doctor::Status::Failed
    );
    std::fs::set_permissions(
        config.join("auth.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let report = inspect(&args, &config, None, None).await;
    assert_eq!(
        check(&report, "config.auth").status,
        withnav::app::doctor::Status::Failed
    );
    std::fs::write(
        config.join("config.json"),
        "{invalid secret-doctor-auth-sentinel",
    )
    .unwrap();
    let report = inspect(&args, &config, None, None).await;
    assert_eq!(report.exit_code(), 1);
    assert!(!report.human().contains(SECRET));
    // The diagnostic document must not bypass the loader's duplicate-key checks.
    std::fs::write(config.join("config.json"), "{}").unwrap();
    std::fs::write(config.join("models.json"), r#"{"providers":{"fake":{"base_url":"http://unused"}},"models":{"fake/chat":{"provider":"fake","model":"chat"},"fake/chat":{"provider":"fake","model":"chat"}}}"#).unwrap();
    let report = inspect(&args, &config, None, None).await;
    assert_eq!(
        check(&report, "config.catalogue").status,
        withnav::app::doctor::Status::Failed
    );
}

#[tokio::test]
async fn live_catalogue_refresh_is_memory_only_and_offline_offer_warns() {
    let (base, calls, task) = server(200, false, Duration::ZERO).await;
    let root = tempfile::tempdir().unwrap();
    let (config, mut args) = fixture(root.path(), &base);
    args.offline = true;
    let path = config.join("models.json");
    let mut models: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    models["providers"]["fake"]["catalog_format"] = json!("omniroute");
    models["models"]["fake/chat"]
        .as_object_mut()
        .unwrap()
        .remove("variants");
    std::fs::write(path, models.to_string()).unwrap();
    let before = snapshot(root.path());
    let report = inspect(&args, &config, None, Some(root.path().join("data"))).await;
    assert_eq!(
        check(&report, "variant.agent.build").status,
        withnav::app::doctor::Status::Warning
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    args.offline = false;
    let report = inspect(&args, &config, None, Some(root.path().join("data"))).await;
    assert_eq!(
        check(&report, "live.metacognition.chat").status,
        withnav::app::doctor::Status::Ok
    );
    assert_eq!(snapshot(root.path()), before);
    task.abort();
}

#[tokio::test]
async fn cli_json_exit_codes_and_trace_logs_never_include_provider_secrets() {
    let (base, _, task) = server(401, false, Duration::ZERO).await;
    let root = tempfile::tempdir().unwrap();
    let (_, args) = fixture(root.path(), &base);
    let before = snapshot(root.path());
    for json in [false, true] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_nav"));
        command
            .args(["doctor", "--layer", "instinct", "--workspace"])
            .arg(&args.workspace)
            .env("XDG_CONFIG_HOME", root.path())
            .env("RUST_LOG", "trace");
        if json {
            command.arg("--json");
        }
        let output = command.output().await.unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        for text in [&stdout, &stderr] {
            assert!(!text.contains(SECRET));
            assert!(!text.contains("password@"));
            assert!(!text.contains(&base));
        }
        assert!(stderr.is_empty(), "{stderr}");
        if json {
            let report: Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(report["schema_version"], 1);
            assert!(report["summary"]["failures"].as_u64().unwrap() > 0);
        }
    }
    assert_eq!(snapshot(root.path()), before);
    task.abort();
}

#[tokio::test]
async fn agent_precedence_parent_inheritance_and_invalid_default() {
    let root = tempfile::tempdir().unwrap();
    let (config, mut args) = fixture(root.path(), "http://127.0.0.1:1/v1");
    args.offline = true;
    std::fs::create_dir_all(config.join("agents")).unwrap();
    std::fs::write(config.join("agents/custom.md"), "---\ndescription: Custom primary\nmode: primary\nmodel: fake/missing\n---\nSynthetic prompt\n").unwrap();
    let config_path = config.join("config.json");
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    settings["agents"]["custom"] = json!({"model":"fake/chat","variant":"off"});
    std::fs::write(&config_path, settings.to_string()).unwrap();
    let report = inspect(&args, &config, None, None).await;
    assert!(check(&report, "assignment.agent.custom")
        .value
        .contains("variant=off (config)"));
    assert!(check(&report, "assignment.agent.mousse")
        .value
        .contains("inherits parent's"));
    settings["agents"].as_object_mut().unwrap().remove("custom");
    std::fs::write(&config_path, settings.to_string()).unwrap();
    let report = inspect(&args, &config, None, None).await;
    assert!(check(&report, "assignment.agent.custom")
        .value
        .contains("unknown"));
    assert_eq!(report.exit_code(), 0);
    settings["default_agent"] = json!("custom");
    std::fs::write(config_path, settings.to_string()).unwrap();
    let report = inspect(&args, &config, None, None).await;
    assert_eq!(report.exit_code(), 1);
    assert_eq!(
        check(&report, "assignment.default_agent").status,
        withnav::app::doctor::Status::Failed
    );
}
