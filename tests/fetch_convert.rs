//! The `fetch` tool converts HTML in a limited child process: these tests
//! run it against the built `withnav` binary with hostile pages.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};
use withnav::tools::fetch::FetchTool;
use withnav::tools::{Tool, ToolContext, ToolResult, ToolStatus};

fn cx() -> ToolContext {
    ToolContext {
        binding: None,
        workspace: PathBuf::from("."),
        writable_roots: Vec::new(),
        readable_roots: Vec::new(),
    }
}

fn tool() -> FetchTool {
    FetchTool::new().with_converter(env!("CARGO_BIN_EXE_nav"))
}

async fn serve(page: impl Into<Vec<u8>>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page.into(), "text/html"))
        .mount(&server)
        .await;
    server
}

async fn fetch(server: &MockServer, args: Value) -> ToolResult {
    let mut args = args;
    args["url"] = json!(server.uri());
    tool()
        .call(args, CancellationToken::new(), &cx())
        .await
        .unwrap()
}

const PAGE: &str = "<html><head><title>T</title><style>p{color:red}</style></head>\
    <body><h1>Heading</h1><p>Some <b>bold</b> text and a <a href=\"https://e.test/x\">link</a>.</p>\
    <script>alert('SCRIPTBODY')</script></body></html>";

// Given a normal page
// When fetched as markdown and as text
// Then the child converts it, with no fallback note
#[tokio::test]
async fn a_normal_page_converts_through_the_child() {
    let server = serve(PAGE).await;

    let md = fetch(&server, json!({})).await;
    assert_eq!(md.status, ToolStatus::Ok, "{:?}", md.error);
    assert!(md.output.contains("# Heading"), "{}", md.output);
    assert!(md.output.contains("**bold**"), "{}", md.output);
    assert!(
        md.output.contains("[link](https://e.test/x)"),
        "{}",
        md.output
    );
    assert!(!md.output.contains("SCRIPTBODY"), "{}", md.output);
    assert!(!md.output.contains("color:red"), "{}", md.output);
    assert!(!md.output.contains("[note:"), "{}", md.output);

    let text = fetch(&server, json!({"format": "text"})).await;
    assert_eq!(text.status, ToolStatus::Ok, "{:?}", text.error);
    assert!(text.output.contains("Some bold text"), "{}", text.output);
    assert!(!text.output.contains("SCRIPTBODY"), "{}", text.output);
    assert!(!text.output.contains("[note:"), "{}", text.output);
}

// Given pages built to defeat a markup pre-check or the DOM renderers
// When fetched
// Then each returns in time, as plain text with a note when the child had to give up
#[tokio::test]
async fn hostile_pages_fall_back_within_the_timeout() {
    let rows = "<tr><td rowspan=\"10000\">x</td></tr>".repeat(10_000);
    let pages = [
        ("self-closing divs", "<div/>".repeat(100_000)),
        (
            "short comment then depth",
            format!("<!-->{}deep", "<div>".repeat(100_000)),
        ),
        ("rowspan table", format!("<table>{rows}</table>")),
    ];
    for (name, page) in pages {
        for format in ["markdown", "text"] {
            let server = serve(page.clone()).await;
            let started = Instant::now();
            let res = fetch(&server, json!({"format": format, "timeout_secs": 15})).await;
            assert!(
                started.elapsed() < Duration::from_secs(14),
                "{name} {format}"
            );
            assert_eq!(
                res.status,
                ToolStatus::Ok,
                "{name} {format}: {:?}",
                res.error
            );
            // htmd copes with the rowspan table by itself; html2text blows its
            // limits and the child is stopped.
            let converted = name == "rowspan table" && format == "markdown";
            assert!(
                converted || res.output.starts_with("[note:"),
                "{name} {format}: {}",
                &res.output[..res.output.len().min(80)]
            );
        }
    }
}

// Given a converter that never answers
// When the call is cancelled
// Then the child process is killed
#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_the_converter() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = dir.path().join("converter.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > {}\nexec /bin/sleep 60\n",
            pidfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let server = serve(PAGE).await;
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    let pid_path = pidfile.clone();
    tokio::spawn(async move {
        while !pid_path.exists()
            || std::fs::read_to_string(&pid_path)
                .unwrap()
                .trim()
                .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        canceller.cancel();
    });
    let res = FetchTool::new()
        .with_converter(&script)
        .call(json!({"url": server.uri()}), cancel, &cx())
        .await
        .unwrap();
    assert_eq!(res.status, ToolStatus::Interrupted);

    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    let proc = PathBuf::from(format!("/proc/{pid}"));
    for _ in 0..100 {
        // Gone, or a zombie waiting for its reaper.
        let state = std::fs::read_to_string(proc.join("stat")).unwrap_or_default();
        if state.is_empty() || state.contains(") Z ") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the converter {pid} is still running");
}

// The default converter is the running executable, without an async runtime.
#[test]
fn nav_converts_and_rejects_malformed_arguments() {
    check_nav_conversion(false);
}

#[cfg(unix)]
#[test]
fn nav_converts_under_the_child_memory_limit() {
    check_nav_conversion(true);
}

fn check_nav_conversion(limited: bool) {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = env!("CARGO_BIN_EXE_nav");
    let mut cmd = Command::new(exe);
    let root = tempfile::tempdir().unwrap();
    cmd.args(["internal-html-convert", "--format", "markdown"])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    common::isolate(&mut cmd, root.path());
    // SAFETY: setrlimit is async-signal-safe and allocates nothing.
    #[cfg(unix)]
    if limited {
        unsafe {
            cmd.pre_exec(|| {
                let limit = 512 * 1024 * 1024;
                nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_AS, limit, limit)
                    .map_err(std::io::Error::from)
            });
        }
    }
    #[cfg(windows)]
    let _ = limited;
    let mut child = cmd.spawn().expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"<h1>Hello</h1><p>a <a href=\"https://x.test/\">link</a></p>")
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{exe}: {:?}", out.status);
    assert!(text.contains("# Hello"), "{exe}: {text}");
    assert!(text.contains("[link](https://x.test/)"), "{exe}: {text}");

    for args in [
        vec!["internal-html-convert", "--format"],
        vec!["internal-html-convert", "--format", "markdown", "extra"],
    ] {
        let mut command = Command::new(exe);
        command.env_clear();
        common::isolate(&mut command, root.path());
        let out = command
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .expect("run malformed conversion child");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}
mod common;
