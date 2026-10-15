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
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};
use withnav::app::{
    doctor::{inspect, DoctorLayer, Report, Status},
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
impl Fake {
    async fn response_error(&self) -> Option<axum::response::Response> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        (self.status != 200).then(|| {
            (
                StatusCode::from_u16(self.status).unwrap(),
                format!("{SECRET} https://user:password@host/key"),
            )
                .into_response()
        })
    }
}
async fn systemone(State(fake): State<Fake>, Json(body): Json<Value>) -> axum::response::Response {
    if let Some(error) = fake.response_error().await {
        return error;
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
    if let Some(error) = fake.response_error().await {
        return error;
    }
    Json(json!({"choices":[{"text":if fake.invalid {""} else {"println!(\"hello\");"}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})).into_response()
}
async fn chat(State(fake): State<Fake>, Json(body): Json<Value>) -> axum::response::Response {
    if let Some(error) = fake.response_error().await {
        return error;
    }
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
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SnapshotEntry {
    path: std::path::PathBuf,
    contents: Vec<u8>,
    mode: u32,
    modified: SystemTime,
}
fn snapshot(root: &Path) -> Vec<SnapshotEntry> {
    let mut entries = vec![];
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let metadata = std::fs::symlink_metadata(entry.path()).unwrap();
        let contents = if metadata.is_dir() {
            entries.extend(snapshot(&entry.path()));
            vec![]
        } else if metadata.is_symlink() {
            std::fs::read_link(entry.path())
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else {
            std::fs::read(entry.path()).unwrap()
        };
        entries.push(SnapshotEntry {
            path: entry.path(),
            contents,
            mode: metadata.permissions().mode(),
            modified: metadata.modified().unwrap(),
        });
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
    assert_eq!(calls.load(Ordering::SeqCst), 4);
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
        (200, false, "unreachable"),
    ] {
        let (base, _, task) = server(status, invalid, Duration::ZERO).await;
        if category == "unreachable" {
            task.abort();
            tokio::task::yield_now().await;
        }
        let root = tempfile::tempdir().unwrap();
        let (config, mut args) = fixture(root.path(), &base);
        for (layer, ids) in [
            (
                DoctorLayer::Reflex,
                &["live.reflex.plain", "live.reflex.context"][..],
            ),
            (DoctorLayer::Instinct, &["live.instinct.editor"][..]),
            (DoctorLayer::Metacognition, &["live.metacognition.chat"][..]),
        ] {
            args.layer = Some(layer);
            let report = inspect(&args, &config, None, None).await;
            assert_eq!(
                report.exit_code(),
                1,
                "{layer:?}/{category}: {}",
                report.human()
            );
            for id in ids {
                let result = check(&report, id);
                assert_eq!(result.status, Status::Failed, "{id}/{category}");
                assert_eq!(
                    result.category.as_deref(),
                    Some(category),
                    "{id}: {}",
                    report.human()
                );
                if status != 200 {
                    assert!(result.value.contains(&status.to_string()));
                }
            }
            let json = serde_json::to_string(&report).unwrap();
            for output in [report.human(), json] {
                assert!(!output.contains(SECRET));
                assert!(!output.contains("password@"));
                assert!(!output.contains(&base));
            }
        }
        task.abort();
    }
}

#[tokio::test]
async fn systemone_two_second_deadline() {
    let (base, _, task) = server(200, false, Duration::from_secs(3)).await;
    let root = tempfile::tempdir().unwrap();
    let (config, mut args) = fixture(root.path(), &base);
    args.layer = Some(withnav::app::doctor::DoctorLayer::Instinct);
    let report = inspect(&args, &config, None, None).await;
    assert_eq!(report.exit_code(), 1);
    let id = "live.instinct.editor";
    assert_eq!(check(&report, id).status, Status::Failed);
    assert_eq!(check(&report, id).category.as_deref(), Some("timeout"));
    assert!((1900..2900).contains(&check(&report, id).elapsed_ms.unwrap()));
    task.abort();
}

#[tokio::test]
async fn fim_and_chat_deadlines_use_virtual_time() {
    for (layer, ids, deadline) in [
        (
            DoctorLayer::Reflex,
            &["live.reflex.plain", "live.reflex.context"][..],
            // The provider's stricter reflex cap fires before doctor's 10 s outer bound.
            Duration::from_millis(withnav::provider::fim::REFLEX_DEADLINE_MS),
        ),
        (
            DoctorLayer::Metacognition,
            &["live.metacognition.chat"][..],
            Duration::from_secs(20),
        ),
    ] {
        let (base, calls, task) = server(200, false, Duration::from_secs(60)).await;
        let root = tempfile::tempdir().unwrap();
        let (config, mut args) = fixture(root.path(), &base);
        args.layer = Some(layer);
        tokio::time::pause();
        let inspection = tokio::spawn(async move { inspect(&args, &config, None, None).await });
        for (index, _) in ids.iter().enumerate() {
            let start = std::time::Instant::now();
            while calls.load(Ordering::SeqCst) < index + 1 {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "{layer:?}: fake request not received"
                );
                assert!(
                    !inspection.is_finished(),
                    "{layer:?}: finished before deadline"
                );
                tokio::task::yield_now().await;
            }
            // Wait for real HTTP I/O before fast-forwarding the clock.
            tokio::time::advance(deadline - Duration::from_millis(1)).await;
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            assert!(!inspection.is_finished(), "{layer:?}: deadline fired early");
            assert_eq!(calls.load(Ordering::SeqCst), index + 1);
            tokio::time::advance(Duration::from_millis(2)).await;
        }
        let start = std::time::Instant::now();
        while !inspection.is_finished() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{layer:?}: deadline did not cancel request"
            );
            tokio::task::yield_now().await;
        }
        let report = inspection.await.unwrap();
        tokio::time::resume();
        assert_eq!(report.exit_code(), 1, "{}", report.human());
        for id in ids {
            assert_eq!(check(&report, id).status, Status::Failed);
            assert_eq!(check(&report, id).category.as_deref(), Some("timeout"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), ids.len());
        task.abort();
    }
}

#[tokio::test]
async fn fixtures_unknown_keys_aliases_permissions_defaults_flows_and_variants() {
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
        "invalid flow",
    )
    .unwrap();
    let before = snapshot(root.path());
    let report = inspect(&args, &config, None, None).await;
    assert_eq!(report.exit_code(), 1, "{}", report.human());
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
    assert_eq!(
        check(&report, "flows.workspace.broken.toml").status,
        Status::Failed
    );
    assert!(!report
        .checks
        .iter()
        .any(|check| check.id.contains("workspace_trust") || check.id.contains("trust_file")));
    // The editor server is nav lsp, including after migration of a legacy path.
    let home = root.path().join("home");
    let settings = home.join(".config/Code/User/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let nav = home.join("nav");
    std::fs::write(&nav, "synthetic existing binary").unwrap();
    for (server_path, expected) in [
        (nav.clone(), Status::Ok),
        (home.join("missing-nav"), Status::Warning),
        (home.join("withnav-lsp"), Status::Ok),
    ] {
        std::fs::write(
            &settings,
            json!({"withnav.serverPath":server_path}).to_string(),
        )
        .unwrap();
        let before = snapshot(root.path());
        let report = inspect(&args, &config, Some(&home), None).await;
        let result = check(
            &report,
            &format!("extension.server_path.{}", settings.display()),
        );
        assert_eq!(result.status, expected, "{}", server_path.display());
        assert_eq!(result.value, server_path.display().to_string());
        assert_eq!(
            check(&report, "binary.nav").value,
            env!("CARGO_PKG_VERSION")
        );
        assert!(!home.join("withnav-lsp").exists());
        assert_eq!(snapshot(root.path()), before);
    }
    std::fs::remove_file(settings).unwrap();
    let before = snapshot(root.path());
    let report = inspect(&args, &config, Some(&home), None).await;
    let result = check(&report, "extension.server_path");
    assert_eq!(result.status, Status::Warning);
    assert!(result.value.contains("not detectable"));
    assert_eq!(snapshot(root.path()), before);
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
    const ENV_SECRET: &str = "private-legacy-env-sentinel";
    let (base, calls, task) = server(401, false, Duration::ZERO).await;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (config, args) = fixture(&home.join(".config"), &base);
    let legacy = home.join(".config/navai");
    let navigator = args.workspace.join(".navigator");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::create_dir_all(&navigator).unwrap();
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_nav"), bin.join("nav")).unwrap();
    let before = snapshot(root.path());
    for layer in ["reflex", "instinct", "metacognition"] {
        for (json, offline) in [(false, false), (true, false), (false, true), (true, true)] {
            let before_calls = calls.load(Ordering::SeqCst);
            let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_nav"));
            command
                .env_clear()
                .args(["doctor", "--layer", layer, "--workspace"])
                .arg(&args.workspace)
                .env("PATH", &bin)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", config.parent().unwrap())
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env("NAVAI_FOO", ENV_SECRET)
                .env("RUST_LOG", "trace");
            if json {
                command.arg("--json");
            }
            if offline {
                command.arg("--offline");
            }
            let output = command.output().await.unwrap();
            assert_eq!(output.status.code(), Some(if offline { 0 } else { 1 }));
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            for text in [&stdout, &stderr] {
                assert!(!text.contains(SECRET));
                assert!(!text.contains(ENV_SECRET));
                assert!(!text.contains("password@"));
                assert!(!text.contains(&base));
            }
            assert!(stderr.is_empty(), "{stderr}");
            if offline {
                assert_eq!(calls.load(Ordering::SeqCst), before_calls);
            }
            assert!(stdout.contains("NAVAI_FOO"));
            assert!(stdout.contains("WITHNAV_FOO"));
            for path in [&legacy, &navigator] {
                assert!(stdout.contains(&format!("legacy.path.{}", path.display())));
            }
            if json {
                let report: Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(report["schema_version"], 1);
                let lsp = report["checks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["id"] == "binary.lsp")
                    .unwrap();
                assert_eq!(lsp["status"], "ok");
                assert!(lsp["value"].as_str().unwrap().contains("matches nav"));
                assert_eq!(
                    report["summary"]["failures"].as_u64().unwrap() > 0,
                    !offline
                );
                let env_check = report["checks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["id"] == "legacy.env.NAVAI_FOO")
                    .unwrap();
                assert_eq!(env_check["status"], "warning");
                assert_eq!(env_check["value"], "NAVAI_FOO");
                for path in [&legacy, &navigator] {
                    let id = format!("legacy.path.{}", path.display());
                    let result = report["checks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|c| c["id"] == id)
                        .unwrap();
                    assert_eq!(result["status"], "warning");
                    assert!(result["hint"]
                        .as_str()
                        .unwrap()
                        .contains(if path == &legacy {
                            "withnav"
                        } else {
                            ".withnav"
                        }));
                }
            }
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

#[tokio::test]
async fn doctor_reports_unreadable_permission_rules_without_changing_the_database() {
    let root = tempfile::tempdir().unwrap();
    let (config, mut args) = fixture(root.path(), "http://127.0.0.1:1");
    args.offline = true;
    let data = root.path().join("data");
    std::fs::create_dir_all(data.join("withnav.db")).unwrap();
    let report = inspect(&args, &config, None, Some(data)).await;
    let permission = check(&report, "permissions.database");
    assert_eq!(permission.status, Status::Warning);
    let text = report.human();
    assert!(
        text.contains("withnav.db") && text.contains("every call is asked"),
        "{text}"
    );
}

#[tokio::test]
async fn doctor_does_not_report_a_non_database_file_as_readable() {
    let root = tempfile::tempdir().unwrap();
    let (config, mut args) = fixture(root.path(), "http://127.0.0.1:1");
    args.offline = true;
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let path = data.join("withnav.db");
    std::fs::write(&path, "this is not a SQLite database").unwrap();
    let report = inspect(&args, &config, None, Some(data)).await;
    assert_eq!(
        check(&report, "permissions.database").status,
        Status::Warning
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "this is not a SQLite database"
    );
}
