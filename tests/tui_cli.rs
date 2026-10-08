//! Binary-level tests of `navai tui` that need no terminal. Every
//! spawned binary gets its own XDG dirs so nothing touches the user's data.
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

struct Run {
    out: Output,
    dirs: TempDir,
}

fn tui(args: &[&str]) -> Run {
    tui_with(args, &[])
}

fn tui_with(args: &[&str], env: &[(&str, &str)]) -> Run {
    let dirs = tempfile::tempdir().expect("tempdir");
    let data = dirs.path().join("data");
    let config = dirs.path().join("config");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(config.join("navai")).unwrap();
    std::fs::write(
        config.join("navai/config.json"),
        r#"{"agents":{"build":{"model":"mock/model"}}}"#,
    )
    .unwrap();
    std::fs::write(config.join("navai/models.json"),r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/model":{"provider":"mock","model":"model","context_window":128000,"supports_tools":true}}}"#).unwrap();
    withnav::provider::auth::set_key(&config.join("navai"), "mock", "test-key").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_nav"));
    command
        .arg("tui")
        .args(args)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", &config)
        .env("HOME", dirs.path())
        .env_remove("NAVAI_SERVER_TOKEN")
        .stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    let out = command.output().expect("run the binary");
    Run { out, dirs }
}

fn stderr(run: &Run) -> String {
    String::from_utf8_lossy(&run.out.stderr).into_owned()
}

fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

#[test]
fn help_lists_every_flag() {
    let run = tui(&["--help"]);
    assert!(run.out.status.success());
    let help = String::from_utf8_lossy(&run.out.stdout);
    for flag in ["--workspace", "--model", "--session", "--models", "--url"] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
    assert!(
        help.contains("NAVAI_SERVER_TOKEN"),
        "the token variable is not documented:\n{help}"
    );
}

#[test]
fn model_and_session_conflict() {
    let run = tui(&["--model", "a", "--session", "b"]);
    assert_eq!(run.out.status.code(), Some(2));
    let err = stderr(&run);
    assert!(
        err.contains("--model") && err.contains("--session"),
        "{err}"
    );
    assert!(err.contains("cannot be used with"), "{err}");
}

#[test]
fn models_and_url_conflict() {
    let run = tui(&["--models", "m.json", "--url", "http://127.0.0.1:1"]);
    assert_eq!(run.out.status.code(), Some(2));
    let err = stderr(&run);
    assert!(err.contains("--models") && err.contains("--url"), "{err}");
}

#[test]
fn url_to_a_closed_port_fails_fast_with_a_readable_error() {
    let url = closed_port_url();
    let started = std::time::Instant::now();
    let run = tui(&["--url", &url, "--workspace", "."]);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(run.out.status.code(), Some(1));
    let err = stderr(&run);
    assert!(err.contains("cannot reach a server"), "{err}");
    assert!(err.contains(&url), "{err}");
    // Nothing of the terminal setup ran: no alternate screen, no raw mode.
    for escape in ["\x1b[?1049h", "\x1b[?2004h"] {
        assert!(!String::from_utf8_lossy(&run.out.stdout).contains(escape));
        assert!(!err.contains(escape));
    }
}

#[test]
fn url_without_a_scheme_is_refused() {
    let run = tui(&["--url", "127.0.0.1:1", "--workspace", "."]);
    assert_eq!(run.out.status.code(), Some(1));
    assert!(stderr(&run).contains("http://"), "{}", stderr(&run));
}

#[test]
fn a_missing_workspace_is_refused_before_anything_starts() {
    let run = tui(&["--workspace", "/nonexistent/for/sure"]);
    assert_eq!(run.out.status.code(), Some(1));
    assert!(stderr(&run).contains("workspace"), "{}", stderr(&run));
}

#[test]
fn without_a_terminal_the_embedded_client_stops_cleanly_and_logs_to_the_file() {
    let workspace = tempfile::tempdir().unwrap();
    let run = tui(&["--workspace", workspace.path().to_str().unwrap()]);
    assert_eq!(run.out.status.code(), Some(1), "{}", stderr(&run));
    assert!(
        stderr(&run).contains("interactive terminal"),
        "{}",
        stderr(&run)
    );
    assert!(run.out.stdout.is_empty(), "stdout must stay clean");
    let log = std::fs::read_to_string(run.dirs.path().join("data").join("navai").join("tui.log"))
        .expect("tui.log exists");
    assert!(log.contains("terminal client started"), "{log}");
}

#[test]
fn the_token_is_never_written_to_the_log() {
    let url = closed_port_url();
    let run = tui_with(
        &["--url", &url, "--workspace", "."],
        &[("NAVAI_SERVER_TOKEN", "super-secret-token-value")],
    );
    let log_dir = run.dirs.path().join("data").join("navai");
    let log = std::fs::read_to_string(log_dir.join("tui.log")).unwrap_or_default();
    assert!(!log.contains("super-secret-token-value"));
    assert!(!stderr(&run).contains("super-secret-token-value"));
}

#[test]
fn the_former_token_variable_works_with_one_deprecation_warning() {
    let url = closed_port_url();
    let run = tui_with(
        &["--url", &url, "--workspace", "."],
        &[("RUST_AGENT_SERVER_TOKEN", "super-secret-token-value")],
    );
    let err = stderr(&run);
    assert_eq!(
        err.matches("RUST_AGENT_SERVER_TOKEN is deprecated").count(),
        1,
        "{err}"
    );
    assert!(err.contains("NAVAI_SERVER_TOKEN"), "{err}");
    assert!(!err.contains("super-secret-token-value"));
}

#[test]
fn the_new_token_variable_prints_no_warning() {
    let url = closed_port_url();
    let run = tui_with(
        &["--url", &url, "--workspace", "."],
        &[("NAVAI_SERVER_TOKEN", "super-secret-token-value")],
    );
    assert!(!stderr(&run).contains("deprecated"), "{}", stderr(&run));
}

#[test]
fn a_start_moves_the_former_data_and_config_once() {
    use std::os::unix::fs::PermissionsExt;
    let dirs = tempfile::tempdir().unwrap();
    let data = dirs.path().join("data");
    let config = dirs.path().join("config");
    std::fs::create_dir_all(data.join("navigator")).unwrap();
    std::fs::create_dir_all(config.join("rust-agent")).unwrap();
    std::fs::write(data.join("navigator/navigator.jsonl"), "log").unwrap();
    let auth = config.join("rust-agent/auth.json");
    std::fs::write(&auth, "{}").unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();

    for _ in 0..2 {
        let status = Command::new(env!("CARGO_BIN_EXE_nav"))
            .args(["tui", "--workspace", "."])
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", &config)
            .env("HOME", dirs.path())
            .stdin(Stdio::null())
            .output()
            .unwrap()
            .status;
        assert_eq!(status.code(), Some(1));
    }

    assert_eq!(
        std::fs::read_to_string(data.join("navai/navai.jsonl")).unwrap(),
        "log"
    );
    assert!(!data.join("navigator/navigator.jsonl").exists());
    let moved = config.join("navai/auth.json");
    assert_eq!(
        std::fs::metadata(&moved).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!auth.exists());
}
