use std::process::Command;
mod common;

fn nav(data: &std::path::Path, workspace: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(&mut command, data);
    command
        .env("XDG_DATA_HOME", data)
        .env("XDG_CONFIG_HOME", data.join("config"))
        .env("HOME", data)
        .current_dir(workspace)
        .args(["permissions"])
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn child_resolves_isolated_roots() {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(&mut command, root.path());
    let output = command
        .args(["doctor", "--offline", "--json"])
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let location = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "config.location")
        .unwrap();
    assert_eq!(
        std::path::PathBuf::from(location["value"].as_str().unwrap()),
        root.path().join("config").join("withnav")
    );
}

#[test]
fn relative_roots_remain_protected_in_another_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut nav = Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(&mut nav, root.path());
    let output = nav
        .current_dir(root.path())
        .env("WITHNAV_CONFIG_DIR", "config/withnav")
        .env("WITHNAV_DATA_DIR", "data/withnav")
        .args(["doctor", "--offline", "--json", "--workspace"])
        .arg(&workspace)
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let location = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "config.location")
        .unwrap();
    assert_eq!(
        location["value"],
        root.path().join("config").join("withnav").to_str().unwrap()
    );

    // Probe the public floor in a fresh process with the same roots and cwd as nav.
    let mut probe = Command::new(std::env::current_exe().unwrap());
    common::isolate(&mut probe, root.path());
    let output = probe
        .current_dir(root.path())
        .env("WITHNAV_CONFIG_DIR", "config/withnav")
        .env("WITHNAV_DATA_DIR", "data/withnav")
        .env("WITHNAV_TEST_FLOOR_WORKSPACE", workspace)
        .args(["--exact", "relative_root_floor_probe"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn relative_root_floor_probe() {
    let Some(workspace) = std::env::var_os("WITHNAV_TEST_FLOOR_WORKSPACE") else {
        return;
    };
    use withnav::permission::floor::{Floor, FloorContext};
    let cx = FloorContext {
        workspace: workspace.into(),
        config_dir: withnav::provider::auth::default_config_dir(),
        data_dir: withnav::paths::data_dir(),
        ..Default::default()
    };
    for target in [
        "config/withnav/config.json",
        "config/withnav/flows/custom.json",
        "data/withnav/withnav.db",
        "data/withnav/withnav.db-wal",
        "data/withnav/withnav.db-shm",
    ] {
        let path = std::env::current_dir().unwrap().join(target);
        let call = withnav::provider::ToolCall {
            id: "probe".into(),
            name: "write".into(),
            args: serde_json::json!({"path":path,"content":"x"}),
            args_valid: true,
        };
        assert!(
            matches!(
                withnav::permission::floor::check(&call, &[], &cx),
                Floor::Protected(_)
            ),
            "{path:?}"
        );
    }
}

#[test]
fn permissions_cli_round_trip() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let _owner_guard = common::owner::OwnerGuard::new(&data.path().join("data/withnav"));
    let run = |args: &[&str]| {
        let output = nav(data.path(), workspace.path(), args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(run(&["list", "--json"]).trim(), "[]");
    run(&["add", "bash", "git status *", "allow"]);
    let rules: serde_json::Value = serde_json::from_str(&run(&["list", "--json"])).unwrap();
    assert_eq!(
        rules[0]["scope"]["project"],
        withnav::tools::canonical_path(workspace.path())
            .unwrap()
            .to_str()
            .unwrap()
    );
    let id = rules[0]["id"].as_i64().unwrap().to_string();
    run(&["revoke", &id]);
    assert_eq!(run(&["list", "--json"]).trim(), "[]");
    let rules: serde_json::Value =
        serde_json::from_str(&run(&["list", "--all", "--json"])).unwrap();
    assert!(rules[0]["revoked_at"].is_string());
    run(&["add", "read", "*", "ask", "--global"]);
    run(&["add", "write", "*", "deny", "--agent", "plan"]);
    for scope in [
        vec!["--global"],
        vec!["--agent", "plan"],
        vec![
            "--project",
            withnav::tools::canonical_path(workspace.path())
                .unwrap()
                .to_str()
                .unwrap(),
        ],
    ] {
        let mut args = vec!["list", "--all", "--json"];
        args.extend(scope);
        let rules: Vec<serde_json::Value> = serde_json::from_str(&run(&args)).unwrap();
        assert_eq!(rules.len(), 1);
    }
    assert_eq!(run(&["history", "--limit", "1", "--json"]).trim(), "[]");
    assert!(
        !nav(
            data.path(),
            workspace.path(),
            &["add", "bash", "*", "allow", "--global", "--agent", "plan"]
        )
        .status
        .success()
    );
    assert!(
        !nav(data.path(), workspace.path(), &["revoke", "99999"])
            .status
            .success()
    );
    assert!(data.path().join("data/withnav/withnav.db").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relative_paths_target_invoking_workspace_when_owner_started_elsewhere() {
    let root = tempfile::tempdir().unwrap();
    let data_home = root.path().join("data");
    let config_home = root.path().join("config");
    let ws_a = root.path().join("workspace_a");
    let ws_b = root.path().join("workspace_b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    std::fs::create_dir_all(config_home.join("withnav")).unwrap();
    std::fs::write(
        config_home.join("withnav/config.json"),
        r#"{"agents":{"build":{"model":"mock/m"}}}"#,
    )
    .unwrap();

    let resp_a = common::sse::text_response("from A");
    let resp_b = common::sse::text_response("from B");
    let mock = common::sse::spawn_mock_provider(vec![resp_a, resp_b]).await;

    let models = root.path().join("models.json");
    std::fs::write(
        &models,
        serde_json::json!({
            "providers": { "mock": {
                "base_url": mock.url,
                "api_key_env": "WITHNAV_TEST_KEY",
                "headers": {}, "connect_timeout_ms": 2000, "request_timeout_ms": 30000
            }},
            "models": { "mock/m": {
                "provider": "mock", "model": "m", "reasoning_history": "preserve",
                "context_window": 128000
            }}
        })
        .to_string(),
    )
    .unwrap();

    let _owner_guard = common::owner::OwnerGuard::new(&data_home.join("withnav"));

    // 1. Start the owner from workspace A using `nav run --workspace .`
    let out_a = {
        let (root_path, models, ws_a, data_home, config_home) = (
            root.path().to_path_buf(),
            models.clone(),
            ws_a.clone(),
            data_home.clone(),
            config_home.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_nav"));
            common::isolate(&mut cmd, &root_path);
            cmd.current_dir(&ws_a)
                .args([
                    "run",
                    "--workspace",
                    ".",
                    "--models",
                    models.to_str().unwrap(),
                    "--prompt",
                    "init in A",
                ])
                .env("WITHNAV_TEST_KEY", "test-key")
                .env("XDG_DATA_HOME", &data_home)
                .env("XDG_CONFIG_HOME", &config_home)
                .env("HOME", &root_path)
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    };
    assert_eq!(out_a.status.code(), Some(0), "out_a: {:?}", out_a);

    // 2. From workspace B, add a rule with `--project .`
    let out_perm = {
        let (root_path, ws_b, data_home, config_home) = (
            root.path().to_path_buf(),
            ws_b.clone(),
            data_home.clone(),
            config_home.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_nav"));
            common::isolate(&mut cmd, &root_path);
            cmd.current_dir(&ws_b)
                .args(["permissions", "add", "read", "*", "allow", "--project", "."])
                .env("XDG_DATA_HOME", &data_home)
                .env("XDG_CONFIG_HOME", &config_home)
                .env("HOME", &root_path)
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    };
    assert_eq!(out_perm.status.code(), Some(0), "out_perm: {:?}", out_perm);

    // Verify the added rule is scoped to canonical(ws_b), not ws_a
    let canonical_b = withnav::tools::canonical_path(&ws_b).unwrap();
    let out_list_b = {
        let (root_path, ws_b, data_home, config_home) = (
            root.path().to_path_buf(),
            ws_b.clone(),
            data_home.clone(),
            config_home.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_nav"));
            common::isolate(&mut cmd, &root_path);
            cmd.current_dir(&ws_b)
                .args(["permissions", "list", "--project", ".", "--json"])
                .env("XDG_DATA_HOME", &data_home)
                .env("XDG_CONFIG_HOME", &config_home)
                .env("HOME", &root_path)
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    };
    assert_eq!(out_list_b.status.code(), Some(0));
    let rules: serde_json::Value = serde_json::from_slice(&out_list_b.stdout).unwrap();
    assert_eq!(rules.as_array().unwrap().len(), 1);
    assert_eq!(rules[0]["scope"]["project"], canonical_b.to_str().unwrap());

    // 3. From workspace B, execute `nav run --workspace .`
    let out_b = {
        let (root_path, models, ws_b, data_home, config_home) = (
            root.path().to_path_buf(),
            models.clone(),
            ws_b.clone(),
            data_home.clone(),
            config_home.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_nav"));
            common::isolate(&mut cmd, &root_path);
            cmd.current_dir(&ws_b)
                .args([
                    "run",
                    "--workspace",
                    ".",
                    "--models",
                    models.to_str().unwrap(),
                    "--prompt",
                    "task in B",
                ])
                .env("WITHNAV_TEST_KEY", "test-key")
                .env("XDG_DATA_HOME", &data_home)
                .env("XDG_CONFIG_HOME", &config_home)
                .env("HOME", &root_path)
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    };
    assert_eq!(out_b.status.code(), Some(0), "out_b: {:?}", out_b);

    // Read owner record and check sessions for workspace B
    let record_bytes = std::fs::read(data_home.join("withnav/server/owner.json")).unwrap();
    let record: serde_json::Value = serde_json::from_slice(&record_bytes).unwrap();
    let port = record["port"].as_u64().unwrap();
    let token = record["token"].as_str().unwrap();

    let client = reqwest::Client::new();
    let list_b_url = format!(
        "http://127.0.0.1:{port}/v1/sessions?workspace={}",
        canonical_b.display()
    );
    let res = client.get(&list_b_url).bearer_auth(token).send().await.unwrap();
    let body: serde_json::Value = res.json().await.unwrap();
    let sessions_b = body["sessions"].as_array().unwrap();
    assert_eq!(sessions_b.len(), 1, "workspace B must have exactly one session");

    let session_id = sessions_b[0]["id"].as_str().unwrap();
    let detail_url = format!("http://127.0.0.1:{port}/v1/sessions/{session_id}");
    let detail: serde_json::Value = client.get(&detail_url).bearer_auth(token).send().await.unwrap().json().await.unwrap();
    assert_eq!(detail["workspace"], canonical_b.to_str().unwrap());
}
