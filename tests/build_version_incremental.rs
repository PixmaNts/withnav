use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .status()
        .expect("git command succeeds");
    assert!(status.success(), "git {:?} failed", args);
}

fn git_head_sha(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .expect("git rev-parse succeeds");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn run_scratch(scratch_dir: &Path, target_dir: &Path) -> String {
    let output = Command::new("cargo")
        .current_dir(scratch_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("CARGO_TARGET_DIR", target_dir)
        .args(["run", "--quiet"])
        .output()
        .expect("cargo run succeeds");
    assert!(
        output.status.success(),
        "cargo run stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("valid utf8 stdout")
}

#[test]
fn incremental_build_invalidation_lifecycle() {
    let td = tempdir().unwrap();
    let root = td.path();

    let build_version_path =
        std::fs::canonicalize(concat!(env!("CARGO_MANIFEST_DIR"), "/build_version.rs"))
            .expect("canonical build_version.rs path");
    // Written into the scratch build script as a string literal: `{:?}` escapes the
    // backslashes of a Windows path, which raw interpolation turned into escape sequences
    // (`\b` in `…\build_version.rs`) and a malformed `#[path]` on the Windows runner.
    let build_version_str = format!(
        "{:?}",
        build_version_path.to_str().expect("valid utf8 path")
    );

    // Initialize git repository
    run_git(root, &["init"]);
    run_git(root, &["config", "user.name", "Test Runner"]);
    run_git(root, &["config", "user.email", "test@example.com"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);

    // Write .gitignore so cargo target directory churn is ignored
    std::fs::write(root.join(".gitignore"), "/target\n").unwrap();

    // Write Cargo.toml
    let cargo_toml = r#"[package]
name = "scratch"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "scratch"
path = "src/main.rs"
"#;
    std::fs::write(root.join("Cargo.toml"), cargo_toml).unwrap();

    // Write src/main.rs
    std::fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = r#"fn main() {
    print!("{}", env!("STAMPED_VERSION"));
}
"#;
    std::fs::write(root.join("src").join("main.rs"), main_rs).unwrap();

    // Write build.rs including real build_version.rs
    let build_rs = format!(
        r#"#[path = {build_version_str}]
mod build_version;

use std::path::Path;

fn main() {{
    let manifest_dir_str = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let manifest_dir = Path::new(&manifest_dir_str);
    let base = std::env::var("CARGO_PKG_VERSION").unwrap();
    build_version::emit_rerun_directives(manifest_dir);
    let version = build_version::version(manifest_dir, &base);
    println!("cargo:rustc-env=STAMPED_VERSION={{version}}");
}}
"#
    );
    std::fs::write(root.join("build.rs"), build_rs).unwrap();

    let target_dir = root.join("target");

    // Run cargo check once to generate Cargo.lock before committing
    let status = Command::new("cargo")
        .current_dir(root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["check", "--quiet"])
        .status()
        .expect("cargo check succeeds");
    assert!(status.success());

    // Commit baseline and tag v0.1.0
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-m", "initial commit"]);
    run_git(root, &["tag", "v0.1.0"]);

    // Step 1: Clean tag -> bare version
    let v1 = run_scratch(root, &target_dir);
    assert_eq!(v1, "0.1.0");

    // Step 2: Unstaged tracked edit -> .dirty
    let main_file = root.join("src").join("main.rs");
    std::fs::write(&main_file, format!("{main_rs}\n// unstaged edit")).unwrap();
    let v2 = run_scratch(root, &target_dir);
    assert_eq!(v2, "0.1.0+dirty");

    // Step 3: Revert -> bare again
    run_git(root, &["checkout", "--", "src/main.rs"]);
    let v3 = run_scratch(root, &target_dir);
    assert_eq!(v3, "0.1.0");

    // Step 4: Staged change -> .dirty
    std::fs::write(&main_file, format!("{main_rs}\n// staged edit")).unwrap();
    run_git(root, &["add", "src/main.rs"]);
    let v4 = run_scratch(root, &target_dir);
    assert_eq!(v4, "0.1.0+dirty");

    // Step 5: Untracked file created -> .dirty
    run_git(root, &["reset", "--hard", "HEAD"]);
    let notes_file = root.join("notes.txt");
    std::fs::write(&notes_file, "untracked note").unwrap();
    let v5 = run_scratch(root, &target_dir);
    assert_eq!(v5, "0.1.0+dirty");

    // Step 6: Untracked removed -> bare
    std::fs::remove_file(&notes_file).unwrap();
    let v6 = run_scratch(root, &target_dir);
    assert_eq!(v6, "0.1.0");

    // Step 7: Checkout -b new branch -> branch format
    run_git(root, &["checkout", "-b", "feature"]);
    let sha = git_head_sha(root);
    let v7 = run_scratch(root, &target_dir);
    assert_eq!(v7, format!("0.1.0-feature+{sha}"));

    // Step 8: Edit on branch -> branch+dirty
    std::fs::write(&main_file, format!("{main_rs}\n// edit on branch")).unwrap();
    let v8 = run_scratch(root, &target_dir);
    assert_eq!(v8, format!("0.1.0-feature+{sha}.dirty"));
}
