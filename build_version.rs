#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Sanitize branch names for SemVer pre-release identifiers.
/// Non-alphanumeric characters become `-`.
/// If the resulting identifier consists purely of ASCII digits,
/// leading zeros are stripped to preserve valid SemVer numeric identifiers
/// (e.g., "001" becomes "1", and "000" becomes "0").
pub fn sanitize_branch(branch: &str) -> String {
    let sanitized: String = branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if !sanitized.is_empty() && sanitized.chars().all(|c| c.is_ascii_digit()) {
        let trimmed = sanitized.trim_start_matches('0');
        if trimmed.is_empty() {
            "0".to_string()
        } else {
            trimmed.to_string()
        }
    } else {
        sanitized
    }
}

fn is_release_branch(branch: Option<&str>) -> bool {
    matches!(branch, None | Some("main" | "master"))
}

/// Pure formatting helper consuming resolved git metadata and dirty status.
pub fn format_version(
    base: &str,
    tag_matches: bool,
    branch: Option<&str>,
    sha: Option<&str>,
    dirty: bool,
) -> String {
    if tag_matches && is_release_branch(branch) {
        if dirty {
            format!("{base}+dirty")
        } else {
            base.to_string()
        }
    } else if let Some(sha) = sha {
        match branch {
            Some(branch) => {
                let sanitized = sanitize_branch(branch);
                if dirty {
                    format!("{base}-{sanitized}+{sha}.dirty")
                } else {
                    format!("{base}-{sanitized}+{sha}")
                }
            }
            None => {
                if dirty {
                    format!("{base}+{sha}.dirty")
                } else {
                    format!("{base}+{sha}")
                }
            }
        }
    } else if dirty {
        format!("{base}+dirty")
    } else {
        base.to_string()
    }
}

fn git_command(manifest_dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(manifest_dir);
    cmd.env_remove("GIT_DIR");
    cmd.env_remove("GIT_WORK_TREE");
    cmd.env_remove("GIT_INDEX_FILE");
    cmd
}

fn repo_toplevel(manifest_dir: &Path) -> Option<PathBuf> {
    let output = git_command(manifest_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

fn is_manifest_repo_root(manifest_dir: &Path) -> bool {
    let Some(toplevel) = repo_toplevel(manifest_dir) else {
        return false;
    };
    toplevel == manifest_dir
        || manifest_dir
            .canonicalize()
            .ok()
            .zip(toplevel.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
}

fn git_output(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let output = git_command(manifest_dir).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(text)
}

fn git_tags_at_head(manifest_dir: &Path) -> Vec<String> {
    let Ok(output) = git_command(manifest_dir)
        .args(["tag", "--points-at", "HEAD"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().map(|line| line.trim().to_string()).collect()
}

fn git_short_sha(manifest_dir: &Path) -> Option<String> {
    git_output(manifest_dir, &["rev-parse", "--short", "HEAD"])
}

fn git_branch(manifest_dir: &Path) -> Option<String> {
    let branch = git_output(manifest_dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" { None } else { Some(branch) }
}

fn git_status_dirty(manifest_dir: &Path) -> bool {
    let Ok(output) = git_command(manifest_dir)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
        ])
        .output()
    else {
        return false;
    };
    output.status.success() && !output.stdout.is_empty()
}

/// Compute the build version string for the package.
pub fn version(manifest_dir: &Path, base: &str) -> String {
    if !is_manifest_repo_root(manifest_dir) {
        return base.to_string();
    }

    let expected_tag = format!("v{base}");
    let tag_matches = git_tags_at_head(manifest_dir)
        .iter()
        .any(|t| t == &expected_tag);
    let branch = git_branch(manifest_dir);
    let sha = git_short_sha(manifest_dir);
    let dirty = git_status_dirty(manifest_dir);

    format_version(base, tag_matches, branch.as_deref(), sha.as_deref(), dirty)
}

fn resolve_git_path(manifest_dir: &Path, name: &str) -> Option<PathBuf> {
    let p_str = git_output(manifest_dir, &["rev-parse", "--git-path", name])?;
    let p = if Path::new(&p_str).is_absolute() {
        PathBuf::from(p_str)
    } else {
        manifest_dir.join(p_str)
    };
    p.exists().then_some(p)
}

/// Discover git references and filesystem paths that should trigger a build rerun.
pub fn rerun_paths(manifest_dir: &Path) -> Vec<String> {
    if !is_manifest_repo_root(manifest_dir) {
        return Vec::new();
    }

    let mut paths = vec![".".to_string()];

    if let Some(git_dir_str) = git_output(manifest_dir, &["rev-parse", "--git-dir"]) {
        let git_dir = if Path::new(&git_dir_str).is_absolute() {
            PathBuf::from(git_dir_str)
        } else {
            manifest_dir.join(git_dir_str)
        };

        let index = git_dir.join("index");
        if index.exists() {
            paths.push(index.to_string_lossy().to_string());
        }

        let head = git_dir.join("HEAD");
        if head.exists() {
            paths.push(head.to_string_lossy().to_string());
            let ref_name = std::fs::read_to_string(&head)
                .ok()
                .and_then(|c| c.trim().strip_prefix("ref: ").map(str::to_owned));
            if let Some(ref_name) = ref_name {
                let resolved = resolve_git_path(manifest_dir, ref_name.trim())
                    .or_else(|| resolve_git_path(manifest_dir, "packed-refs"));
                if let Some(p) = resolved {
                    paths.push(p.to_string_lossy().to_string());
                }
            }
        }
    }

    for name in ["HEAD", "refs/heads", "refs/tags", "packed-refs"] {
        if let Some(p) = resolve_git_path(manifest_dir, name) {
            paths.push(p.to_string_lossy().to_string());
        }
    }

    let mut unique = Vec::new();
    for p in paths {
        if !unique.contains(&p) {
            unique.push(p);
        }
    }
    unique
}

/// Emit cargo:rerun-if-changed directives for git invalidation tracking.
pub fn emit_rerun_directives(manifest_dir: &Path) {
    for path in rerun_paths(manifest_dir) {
        println!("cargo:rerun-if-changed={path}");
    }
}
