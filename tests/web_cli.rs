//! Subprocess tests for `nav web`.

use std::io::Read;
use std::net::TcpListener;
use std::process::Command;

mod common;

#[test]
fn web_cli_no_proxy_leak_and_prints_pairing_address() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy listener");
    let proxy_addr = listener.local_addr().expect("proxy addr");
    listener.set_nonblocking(true).expect("set nonblocking");

    let root = tempfile::tempdir().expect("tempdir");
    let data = root.path().join("data/withnav");
    let config = root.path().join("config/withnav");
    std::fs::create_dir_all(&data).expect("create data dir");
    std::fs::create_dir_all(&config).expect("create config dir");
    std::fs::write(
        config.join("config.json"),
        r#"{"agents":{"build":{"model":"mock/model"}}}"#,
    )
    .expect("write config.json");
    std::fs::write(
        config.join("models.json"),
        r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/model":{"provider":"mock","model":"model","context_window":128000,"supports_tools":true}}}"#,
    )
    .expect("write models.json");
    withnav::provider::auth::set_key(&config, "mock", "test-key").expect("set key");

    let _guard = common::owner::OwnerGuard::new(&data);

    let proxy_url = format!("http://{proxy_addr}");
    let mut command = Command::new(env!("CARGO_BIN_EXE_nav"));
    common::isolate(&mut command, root.path());
    command
        .arg("web")
        .arg("--no-open")
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("HOME", root.path())
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url);

    let output = command.output().expect("run nav web");
    assert!(
        output.status.success(),
        "nav web failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify proxy listener received no bytes
    let mut proxy_bytes = Vec::new();
    while let Ok((mut stream, _)) = listener.accept() {
        let mut buf = [0u8; 1024];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            proxy_bytes.extend_from_slice(&buf[..n]);
        }
    }
    assert!(
        proxy_bytes.is_empty(),
        "proxy received leaked bytes: {}",
        String::from_utf8_lossy(&proxy_bytes)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/#pair="),
        "expected #pair= in stdout: {stdout}"
    );

    let owner_record_path = data.join("server/owner.json");
    let owner_bytes = std::fs::read(&owner_record_path).expect("read owner.json");
    let record: withnav::server::owner::Record =
        serde_json::from_slice(&owner_bytes).expect("parse owner record");
    assert!(
        !stdout.contains(&record.token),
        "stdout must not contain owner token"
    );
    assert!(!stdout.contains("token="), "stdout must not contain token=");
}
