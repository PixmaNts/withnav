/// The fixture interpreter, overridable for machines with multiple installations.
pub fn python() -> String {
    let interpreter = std::env::var("WITHNAV_TEST_PYTHON")
        .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.to_owned());
    // Resolve before MCP applies its isolated child environment and configured Path.
    let output = std::process::Command::new(&interpreter)
        .args(["-X", "utf8", "-c", "import sys; print(sys.executable)"])
        .output()
        .expect("test Python interpreter must be installed");
    assert!(
        output.status.success(),
        "test Python interpreter must execute (set WITHNAV_TEST_PYTHON)"
    );
    String::from_utf8(output.stdout)
        .expect("Python executable path must be UTF-8")
        .trim()
        .to_owned()
}
