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
        location["value"],
        root.path().join("config").join("withnav").to_str().unwrap()
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
        workspace.path().to_str().unwrap()
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
        vec!["--project", workspace.path().to_str().unwrap()],
    ] {
        let mut args = vec!["list", "--all", "--json"];
        args.extend(scope);
        let rules: Vec<serde_json::Value> = serde_json::from_str(&run(&args)).unwrap();
        assert_eq!(rules.len(), 1);
    }
    assert_eq!(run(&["history", "--limit", "1", "--json"]).trim(), "[]");
    assert!(!nav(
        data.path(),
        workspace.path(),
        &["add", "bash", "*", "allow", "--global", "--agent", "plan"]
    )
    .status
    .success());
    assert!(!nav(data.path(), workspace.path(), &["revoke", "99999"])
        .status
        .success());
    assert!(data.path().join("data/withnav/withnav.db").exists());
}
