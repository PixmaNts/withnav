// The WebUI is embedded from `webui/dist` with `include_dir!`, which cannot tell
// cargo to watch that folder on stable Rust: without this line a rebuilt `dist`
// would not trigger a rebuild and `nav web` would keep serving the old page.
fn main() {
    println!("cargo:rerun-if-changed=webui/dist");
}
