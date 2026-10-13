/// The fixture interpreter, overridable for machines with multiple installations.
pub fn python() -> String {
    std::env::var("WITHNAV_TEST_PYTHON")
        .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.to_owned())
}
