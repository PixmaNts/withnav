#[path = "../build_version.rs"]
mod build_version;

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

const SEMVER_PATTERN: &str = r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$";

fn is_valid_semver(version: &str) -> bool {
    let re = regex::Regex::new(SEMVER_PATTERN).expect("valid regex");
    re.is_match(version)
}

#[test]
fn pure_formatting_all_six_formats() {
    let base = "0.8.2";
    let sha = "abc1234";
    let branch = "develop";

    // 1. Tagged clean
    let v_tag_clean = build_version::format_version(base, true, None, Some(sha), false);
    assert_eq!(v_tag_clean, "0.8.2");
    assert!(is_valid_semver(&v_tag_clean));

    // 2. Tagged dirty
    let v_tag_dirty = build_version::format_version(base, true, None, Some(sha), true);
    assert_eq!(v_tag_dirty, "0.8.2+dirty");
    assert!(is_valid_semver(&v_tag_dirty));

    // 3. Branch clean
    let v_branch_clean = build_version::format_version(base, false, Some(branch), Some(sha), false);
    assert_eq!(v_branch_clean, "0.8.2-develop+abc1234");
    assert!(is_valid_semver(&v_branch_clean));

    // 4. Branch dirty
    let v_branch_dirty = build_version::format_version(base, false, Some(branch), Some(sha), true);
    assert_eq!(v_branch_dirty, "0.8.2-develop+abc1234.dirty");
    assert!(is_valid_semver(&v_branch_dirty));

    // 5. Detached clean
    let v_detached_clean = build_version::format_version(base, false, None, Some(sha), false);
    assert_eq!(v_detached_clean, "0.8.2+abc1234");
    assert!(is_valid_semver(&v_detached_clean));

    // 6. Detached dirty
    let v_detached_dirty = build_version::format_version(base, false, None, Some(sha), true);
    assert_eq!(v_detached_dirty, "0.8.2+abc1234.dirty");
    assert!(is_valid_semver(&v_detached_dirty));
}

#[test]
fn pure_formatting_numeric_branch_normalization() {
    let base = "0.8.2";
    let sha = "abc1234";

    // Purely numeric branch with leading zeros: stripped to valid SemVer
    let v_001 = build_version::format_version(base, false, Some("001"), Some(sha), false);
    assert_eq!(v_001, "0.8.2-1+abc1234");
    assert!(is_valid_semver(&v_001));

    let v_001_dirty = build_version::format_version(base, false, Some("001"), Some(sha), true);
    assert_eq!(v_001_dirty, "0.8.2-1+abc1234.dirty");
    assert!(is_valid_semver(&v_001_dirty));

    let v_000 = build_version::format_version(base, false, Some("000"), Some(sha), false);
    assert_eq!(v_000, "0.8.2-0+abc1234");
    assert!(is_valid_semver(&v_000));

    let v_0 = build_version::format_version(base, false, Some("0"), Some(sha), false);
    assert_eq!(v_0, "0.8.2-0+abc1234");
    assert!(is_valid_semver(&v_0));

    let v_42 = build_version::format_version(base, false, Some("42"), Some(sha), false);
    assert_eq!(v_42, "0.8.2-42+abc1234");
    assert!(is_valid_semver(&v_42));

    // Alphanumeric branches retain leading zeros since SemVer allows leading zeros in non-numeric segments
    let v_alphanumeric =
        build_version::format_version(base, false, Some("001-fix"), Some(sha), false);
    assert_eq!(v_alphanumeric, "0.8.2-001-fix+abc1234");
    assert!(is_valid_semver(&v_alphanumeric));

    let v_slash = build_version::format_version(base, false, Some("fix/001"), Some(sha), false);
    assert_eq!(v_slash, "0.8.2-fix-001+abc1234");
    assert!(is_valid_semver(&v_slash));
}

#[test]
fn pure_formatting_long_branch_near_doctor_limit() {
    let base = "0.8.2";
    let sha = "1234567";
    // Construct a branch name such that the total version length approaches 128 chars
    let branch = "a".repeat(105);
    let v = build_version::format_version(base, false, Some(&branch), Some(sha), true);
    // "0.8.2-" (6) + 105 + "+1234567.dirty" (14) = 125 chars
    assert_eq!(v.len(), 125);
    assert!(v.len() < 128);
    assert!(
        v.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c))
    );
    assert!(is_valid_semver(&v));
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .status()
        .expect("git invocation succeeds");
    assert!(status.success(), "git {:?} failed in {:?}", args, dir);
}

fn init_git_repo(dir: &Path) {
    run_git(dir, &["init"]);
    run_git(dir, &["config", "user.name", "Test Runner"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
}

fn commit_file(dir: &Path, rel_path: &str, content: &str, msg: &str) {
    let path = dir.join(rel_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
    run_git(dir, &["add", rel_path]);
    run_git(dir, &["commit", "-m", msg]);
}

fn git_head_sha(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn temp_repo_integration_tagged_clean_and_dirty() {
    let td = tempdir().unwrap();
    let root = td.path();
    init_git_repo(root);
    commit_file(root, "file.txt", "initial", "initial commit");
    run_git(root, &["tag", "v0.8.2"]);

    // 1. Tagged clean
    let v = build_version::version(root, "0.8.2");
    assert_eq!(v, "0.8.2");

    // 2. Untracked file -> dirty
    let untracked = root.join("untracked.txt");
    std::fs::write(&untracked, "new file").unwrap();
    let v_untracked = build_version::version(root, "0.8.2");
    assert_eq!(v_untracked, "0.8.2+dirty");

    // 3. Remove untracked file -> clean again
    std::fs::remove_file(&untracked).unwrap();
    let v_restored = build_version::version(root, "0.8.2");
    assert_eq!(v_restored, "0.8.2");

    // 4. Tracked edit unstaged -> dirty
    std::fs::write(root.join("file.txt"), "modified").unwrap();
    let v_unstaged = build_version::version(root, "0.8.2");
    assert_eq!(v_unstaged, "0.8.2+dirty");

    // 5. Staged change -> dirty
    run_git(root, &["add", "file.txt"]);
    let v_staged = build_version::version(root, "0.8.2");
    assert_eq!(v_staged, "0.8.2+dirty");

    // 6. Revert restores clean
    run_git(root, &["reset", "--hard", "HEAD"]);
    let v_reverted = build_version::version(root, "0.8.2");
    assert_eq!(v_reverted, "0.8.2");
}

#[test]
fn temp_repo_integration_branch_clean_and_dirty() {
    let td = tempdir().unwrap();
    let root = td.path();
    init_git_repo(root);
    commit_file(root, "file.txt", "initial", "initial commit");
    run_git(root, &["checkout", "-b", "feature/my-branch"]);
    let sha = git_head_sha(root);

    // Branch clean
    let v_clean = build_version::version(root, "0.8.2");
    assert_eq!(v_clean, format!("0.8.2-feature-my-branch+{sha}"));

    // Branch dirty
    std::fs::write(root.join("file.txt"), "modified").unwrap();
    let v_dirty = build_version::version(root, "0.8.2");
    assert_eq!(v_dirty, format!("0.8.2-feature-my-branch+{sha}.dirty"));
}

#[test]
fn temp_repo_integration_detached_clean_and_dirty() {
    let td = tempdir().unwrap();
    let root = td.path();
    init_git_repo(root);
    commit_file(root, "file.txt", "initial", "initial commit");
    let sha = git_head_sha(root);
    run_git(root, &["checkout", "--detach", "HEAD"]);

    // Detached clean
    let v_clean = build_version::version(root, "0.8.2");
    assert_eq!(v_clean, format!("0.8.2+{sha}"));

    // Detached dirty
    std::fs::write(root.join("file.txt"), "dirty").unwrap();
    let v_dirty = build_version::version(root, "0.8.2");
    assert_eq!(v_dirty, format!("0.8.2+{sha}.dirty"));
}

#[test]
fn temp_repo_integration_no_repo_fallback() {
    let td = tempdir().unwrap();
    let root = td.path();
    // Directory without git init
    let v = build_version::version(root, "0.8.2");
    assert_eq!(v, "0.8.2");
}

#[test]
fn temp_repo_integration_wrong_ancestor_fallback() {
    let td = tempdir().unwrap();
    let root = td.path();
    init_git_repo(root);
    commit_file(root, "file.txt", "initial", "initial commit");

    // Subdirectory inside the repo that is not the repo root
    let sub = root.join("crates").join("nested");
    std::fs::create_dir_all(&sub).unwrap();

    let v = build_version::version(&sub, "0.8.2");
    assert_eq!(v, "0.8.2");
}

#[test]
fn helper_child_probe() {
    let Ok(manifest_dir) = std::env::var("WITHNAV_PROBE_MANIFEST_DIR") else {
        return;
    };
    let base = std::env::var("WITHNAV_PROBE_BASE").unwrap_or_else(|_| "0.8.2".to_string());
    let v = build_version::version(Path::new(&manifest_dir), &base);
    println!("PROBE_VERSION_RESULT:{v}");
}

#[test]
fn env_contamination_isolated_from_contradictory_git_vars() {
    let td_a = tempdir().unwrap();
    let root_a = td_a.path();
    init_git_repo(root_a);
    commit_file(root_a, "file.txt", "repo a", "initial a");
    run_git(root_a, &["tag", "v0.8.2"]);

    let td_b = tempdir().unwrap();
    let root_b = td_b.path();
    init_git_repo(root_b);
    commit_file(root_b, "file.txt", "repo b", "initial b");
    run_git(root_b, &["checkout", "-b", "contradictory-branch"]);
    std::fs::write(root_b.join("file.txt"), "dirty b").unwrap();

    let exe = std::env::current_exe().expect("current test binary path");
    let output = Command::new(exe)
        .args(["--exact", "helper_child_probe", "--nocapture"])
        .env("WITHNAV_PROBE_MANIFEST_DIR", root_a)
        .env("WITHNAV_PROBE_BASE", "0.8.2")
        .env("GIT_DIR", root_b.join(".git"))
        .env("GIT_WORK_TREE", root_b)
        .env("GIT_INDEX_FILE", root_b.join(".git").join("index"))
        .output()
        .expect("child execution succeeds");

    assert!(
        output.status.success(),
        "child stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let res = stdout
        .lines()
        .find_map(|l| l.strip_prefix("PROBE_VERSION_RESULT:"))
        .expect("probe result found");
    // Must match repo A's clean tag version, ignoring contradictory env variables
    assert_eq!(res, "0.8.2");
}

#[test]
fn git_unavailable_fallback_in_child_process() {
    let td = tempdir().unwrap();
    let root = td.path();
    init_git_repo(root);
    commit_file(root, "file.txt", "initial", "initial commit");
    run_git(root, &["checkout", "-b", "develop"]);

    let exe = std::env::current_exe().expect("current test binary path");
    let output = Command::new(exe)
        .args(["--exact", "helper_child_probe", "--nocapture"])
        .env("WITHNAV_PROBE_MANIFEST_DIR", root)
        .env("WITHNAV_PROBE_BASE", "0.8.2")
        .env("PATH", "/dev/null")
        .output()
        .expect("child execution succeeds");

    assert!(
        output.status.success(),
        "child stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let res = stdout
        .lines()
        .find_map(|l| l.strip_prefix("PROBE_VERSION_RESULT:"))
        .expect("probe result found");
    assert_eq!(res, "0.8.2");
}

#[test]
fn rerun_paths_emits_expected_watch_set() {
    let td = tempdir().unwrap();
    let root = td.path();
    init_git_repo(root);
    commit_file(root, "file.txt", "initial", "initial commit");
    run_git(root, &["checkout", "-b", "my-branch"]);

    let paths = build_version::rerun_paths(root);
    assert!(paths.contains(&".".to_string()));

    let git_dir: PathBuf = root.join(".git");
    let index = git_dir.join("index");
    let head = git_dir.join("HEAD");
    let branch_ref = git_dir.join("refs").join("heads").join("my-branch");

    // Compared as paths: git answers `--git-path` with `/` separators, which Windows joins
    // after a `\` prefix; cargo reads either.
    let watched = |path: &PathBuf| paths.iter().any(|p| std::path::Path::new(p) == path);
    assert!(watched(&index), "{paths:?}");
    assert!(watched(&head), "{paths:?}");
    assert!(watched(&branch_ref), "{paths:?}");
}
