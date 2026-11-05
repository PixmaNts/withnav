// The WebUI is embedded from `webui/dist` with `include_dir!`, which cannot tell
// cargo to watch that folder on stable Rust: without this line a rebuilt `dist`
// would not trigger a rebuild and `nav web` would keep serving the old page.
fn main() {
    println!("cargo:rerun-if-changed=webui/dist");
    // `--git-path` resolves to the real files, also in a worktree, where `.git` is a file.
    for path in ["HEAD", "refs/heads", "refs/tags", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rustc-env=WITHNAV_VERSION={}", version());
}

/// `0.8.1` on its release tag `v0.8.1` or outside git, `0.8.1-<branch>+<sha>`
/// on a branch, `0.8.1+<sha>` on a detached HEAD. Semver: `/` and other
/// characters a pre-release cannot hold become `-`.
fn version() -> String {
    let base = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let tags = git(&["tag", "--points-at", "HEAD"]).unwrap_or_default();
    if tags.lines().any(|tag| tag.trim() == format!("v{base}")) {
        return base;
    }
    let Some(sha) = git(&["rev-parse", "--short", "HEAD"]) else {
        return base;
    };
    match git(&["rev-parse", "--abbrev-ref", "HEAD"]).filter(|branch| branch != "HEAD") {
        Some(branch) => {
            let branch: String = branch
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            format!("{base}-{branch}+{sha}")
        }
        None => format!("{base}+{sha}"),
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !text.is_empty()).then_some(text)
}
