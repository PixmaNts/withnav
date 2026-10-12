use std::process::Command;

fn nav(data: &std::path::Path, workspace: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nav"))
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
    assert!(data.path().join("withnav/withnav.db").exists());
}
