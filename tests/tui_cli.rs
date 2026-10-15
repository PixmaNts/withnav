//! Binary-level tests of `nav tui` that need no terminal. Every
//! spawned binary gets its own XDG dirs so nothing touches the user's data.
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;
mod common;

struct Run {
    out: Output,
    dirs: TempDir,
}

fn tui(args: &[&str]) -> Run {
    tui_with(args, &[])
}

fn tui_with(args: &[&str], env: &[(&str, &str)]) -> Run {
    let (mut command, dirs) = tui_command(args, env);
    let out = command.output().expect("run the binary");
    Run { out, dirs }
}

fn tui_command(args: &[&str], env: &[(&str, &str)]) -> (Command, TempDir) {
    let dirs = tempfile::tempdir().expect("tempdir");
    let data = dirs.path().join("data");
    let config = dirs.path().join("config");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(config.join("withnav")).unwrap();
    std::fs::write(
        config.join("withnav/config.json"),
        r#"{"agents":{"build":{"model":"mock/model"}}}"#,
    )
    .unwrap();
    std::fs::write(config.join("withnav/models.json"),r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/model":{"provider":"mock","model":"model","context_window":128000,"supports_tools":true}}}"#).unwrap();
    withnav::provider::auth::set_key(&config.join("withnav"), "mock", "test-key").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(&mut command, dirs.path());
    command
        .arg("tui")
        .args(args)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", &config)
        .env("HOME", dirs.path())
        .env_remove("WITHNAV_SERVER_TOKEN")
        .env_remove("NAVAI_SERVER_TOKEN")
        .stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    (command, dirs)
}

fn stderr(run: &Run) -> String {
    String::from_utf8_lossy(&run.out.stderr).into_owned()
}

async fn unavailable_server() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/health"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    server
}

async fn tui_unavailable(server: &wiremock::MockServer, env: &[(&str, &str)]) -> Run {
    let url = server.uri();
    let (command, dirs) = tui_command(&["--url", &url, "--workspace", "."], env);
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::from(command)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("unavailable server must fail within a bounded wait")
    .expect("run the binary");
    Run { out, dirs }
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
        help.contains("WITHNAV_SERVER_TOKEN"),
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

#[tokio::test]
async fn url_to_an_unavailable_server_fails_fast_with_a_readable_error() {
    let server = unavailable_server().await;
    let url = server.uri();
    let run = tui_unavailable(&server, &[]).await;
    assert_eq!(run.out.status.code(), Some(1));
    let err = stderr(&run);
    assert!(err.contains("cannot reach a server"), "{err}");
    assert!(err.contains(&url), "{err}");
    assert!(err.contains("HTTP 503"), "{err}");
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
    let log = std::fs::read_to_string(run.dirs.path().join("data").join("withnav").join("tui.log"))
        .expect("tui.log exists");
    assert!(log.contains("terminal client started"), "{log}");
}

#[tokio::test]
async fn the_token_is_never_written_to_the_log() {
    let server = unavailable_server().await;
    let run = tui_unavailable(
        &server,
        &[("WITHNAV_SERVER_TOKEN", "super-secret-token-value")],
    )
    .await;
    let log_dir = run.dirs.path().join("data").join("withnav");
    let log = std::fs::read_to_string(log_dir.join("tui.log")).unwrap_or_default();
    assert!(!log.contains("super-secret-token-value"));
    assert!(!stderr(&run).contains("super-secret-token-value"));
}

#[tokio::test]
async fn the_former_token_variable_works_with_one_deprecation_warning() {
    let server = unavailable_server().await;
    let run = tui_unavailable(
        &server,
        &[("RUST_AGENT_SERVER_TOKEN", "super-secret-token-value")],
    )
    .await;
    let err = stderr(&run);
    assert_eq!(
        err.matches("RUST_AGENT_SERVER_TOKEN is deprecated").count(),
        1,
        "{err}"
    );
    assert!(err.contains("WITHNAV_SERVER_TOKEN"), "{err}");
    assert!(!err.contains("super-secret-token-value"));
}

#[tokio::test]
async fn the_new_token_variable_prints_no_warning() {
    let server = unavailable_server().await;
    let run = tui_unavailable(
        &server,
        &[("WITHNAV_SERVER_TOKEN", "super-secret-token-value")],
    )
    .await;
    assert!(!stderr(&run).contains("deprecated"), "{}", stderr(&run));
}

#[tokio::test]
async fn the_navai_token_variable_still_works_with_a_deprecation_warning() {
    let server = unavailable_server().await;
    let run = tui_unavailable(
        &server,
        &[("NAVAI_SERVER_TOKEN", "super-secret-token-value")],
    )
    .await;
    // The terminal owns stderr, so the deprecation goes to the client log.
    let log =
        std::fs::read_to_string(run.dirs.path().join("data/withnav/tui.log")).unwrap_or_default();
    assert!(
        log.contains("NAVAI_SERVER_TOKEN is deprecated; use WITHNAV_SERVER_TOKEN"),
        "{log}"
    );
    assert!(!log.contains("super-secret-token-value"));
    assert!(!stderr(&run).contains("super-secret-token-value"));
}
