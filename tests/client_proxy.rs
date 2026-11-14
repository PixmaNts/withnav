//! The CLI commands that talk to the WithNav server reach it directly on loopback: a configured
//! `HTTP_PROXY`/`ALL_PROXY` never receives a connection, so it can never see the server's token.

use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

mod common;

/// A listener standing in for a proxy; counts the connections it receives and their bytes.
struct RecordingProxy {
    listener: TcpListener,
}

impl RecordingProxy {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        listener.set_nonblocking(true).expect("nonblocking");
        Self { listener }
    }

    fn url(&self) -> String {
        format!("http://{}", self.listener.local_addr().unwrap())
    }

    /// Every connection received so far, with what it sent.
    fn received(&self) -> Vec<Vec<u8>> {
        let mut connections = Vec::new();
        while let Ok((mut stream, _)) = self.listener.accept() {
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let mut bytes = Vec::new();
            let _ = stream.read_to_end(&mut bytes);
            connections.push(bytes);
        }
        connections
    }
}

fn workspace(root: &Path) -> std::path::PathBuf {
    let config = root.join("config/withnav");
    std::fs::create_dir_all(root.join("data/withnav")).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.json"),
        r#"{"agents":{"build":{"model":"mock/model"}}}"#,
    )
    .unwrap();
    std::fs::write(
        config.join("models.json"),
        r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/model":{"provider":"mock","model":"model","context_window":128000,"supports_tools":true}}}"#,
    )
    .unwrap();
    withnav::provider::auth::set_key(&config, "mock", "test-key").unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    workspace
}

fn nav(root: &Path, workspace: &Path, proxy: &str, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(&mut command, root);
    command
        .args(args)
        .current_dir(workspace)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        command.env(name, proxy);
    }
    // A request sent to the recording proxy is never answered: bound the command so a leak is
    // reported by the proxy assertion instead of hanging the test.
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("run nav");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while child.try_wait().expect("wait nav").is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().expect("collect nav output")
}

// Given HTTP_PROXY and ALL_PROXY pointing at a recording listener
// When `nav permissions list` starts the server and asks it, then `nav doctor` reports on it
// Then both commands succeed and the listener never receives a connection
#[test]
fn server_requests_from_permissions_and_doctor_never_use_a_proxy() {
    let proxy = RecordingProxy::start();
    let root = tempfile::tempdir().unwrap();
    let workspace = workspace(root.path());
    let _guard = common::owner::OwnerGuard::new(&root.path().join("data/withnav"));

    let listed = nav(
        root.path(),
        &workspace,
        &proxy.url(),
        &["permissions", "list"],
    );
    assert_no_connection(&proxy);
    assert!(
        listed.status.success(),
        "nav permissions list: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let doctor = nav(
        root.path(),
        &workspace,
        &proxy.url(),
        &["doctor", "--offline", "--json"],
    );
    let report = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        report.contains("server.owner") && report.contains("on port"),
        "doctor did not report the running server: {report}"
    );

    assert_no_connection(&proxy);
}

fn assert_no_connection(proxy: &RecordingProxy) {
    let received = proxy.received();
    assert!(
        received.is_empty(),
        "the proxy received {} connection(s): {:?}",
        received.len(),
        received
            .iter()
            // The request line only: the headers would carry the server's token.
            .map(|bytes| String::from_utf8_lossy(bytes)
                .lines()
                .next()
                .unwrap_or("")
                .to_owned())
            .collect::<Vec<_>>()
    );
}
