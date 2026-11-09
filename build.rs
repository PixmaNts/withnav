#[path = "build_version.rs"]
mod build_version;

use std::path::Path;

// The WebUI is embedded from `webui/dist` with `include_dir!`, which cannot tell
// cargo to watch that folder on stable Rust: without this line a rebuilt `dist`
// would not trigger a rebuild and `nav web` would keep serving the old page.
fn main() {
    println!("cargo:rerun-if-changed=webui/dist");
    let manifest_dir_str =
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let manifest_dir = Path::new(&manifest_dir_str);
    let base = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    build_version::emit_rerun_directives(manifest_dir);
    let version = build_version::version(manifest_dir, &base);
    println!("cargo:rustc-env=WITHNAV_VERSION={version}");
}
