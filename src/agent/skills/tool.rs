//! The `skill` tool: loads one skill by its exact id.
//!
//! Success returns the body between a header naming the file and its
//! directory, then the relative paths of the files next to it (the model
//! opens them with `read`/`grep`, which accept skill roots for reading
//! only). Failures are structured errors with a stable `code`:
//! `skill_unknown`, `skill_ambiguous`, `skill_disabled`, `skill_unreadable`
//! and `skill_oversized`.

use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{frontmatter, Resolution, Skill, Skills, MAX_DEPTH, SKIPPED_DIRS};
use crate::error::ToolError;
use crate::tools::{Tool, ToolContext, ToolResult};

/// The largest body the tool returns; a bigger one is an error, not a cut.
pub const MAX_BODY_BYTES: usize = 32 * 1024;
/// The most resource paths listed.
pub const MAX_RESOURCES: usize = 64;
/// The most directory entries looked at to list resources.
pub(super) const MAX_RESOURCE_ENTRIES: usize = 5_000;

pub struct SkillTool {
    skills: Arc<Skills>,
}

impl SkillTool {
    pub fn new(skills: Arc<Skills>) -> Self {
        Self { skills }
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn schema(&self) -> Value {
        json!({
            "description": "Load a skill by its exact id from the skills catalog. Returns its instructions and the files next to it. Loading does not run anything.",
            "parameters": {
                "type": "object",
                "properties": {"name": {"type": "string", "description": "the skill id, exactly as listed"}},
                "required": ["name"]
            }
        })
    }

    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        _cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(name) = args.get("name").and_then(Value::as_str) else {
                return Err(ToolError::Denied {
                    code: "invalid_args".into(),
                    message: "missing string argument: name".into(),
                });
            };
            let skill = match self.skills.resolve(name) {
                Resolution::Found(skill) => skill.clone(),
                Resolution::Unknown => {
                    return Ok(ToolResult::error(
                        "skill_unknown",
                        format!("no skill with the id {name:?}; use an id from the catalog"),
                    ))
                }
                Resolution::Disabled => {
                    return Ok(ToolResult::error(
                        "skill_disabled",
                        format!("the skill {name:?} may not be loaded by the model"),
                    ))
                }
                Resolution::Ambiguous(paths) => {
                    let listed: Vec<_> = paths.iter().map(|p| p.display().to_string()).collect();
                    return Ok(ToolResult::error(
                        "skill_ambiguous",
                        format!(
                            "several skills share the id {name:?} at the same priority: {}",
                            listed.join(", ")
                        ),
                    ));
                }
            };
            let roots = self.skills.readable_roots().to_vec();
            Ok(tokio::task::spawn_blocking(move || load(&skill, &roots))
                .await
                .unwrap_or_else(|e| ToolResult::error("skill_unreadable", e.to_string())))
        })
    }
}

/// Whether `path` still resolves, right now, to `expected` inside one of the
/// skill `roots` the snapshot was taken from, and has the wanted kind. The
/// paths cached at discovery are not trusted: a file, a directory or any
/// ancestor may have been replaced by a link since.
fn still_in_place(
    path: &Path,
    expected: &Path,
    roots: &[PathBuf],
    dir: bool,
) -> Result<(), String> {
    let now = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    if !roots.iter().any(|root| now.starts_with(root)) {
        return Err(format!(
            "it now resolves outside the skill roots, to {}",
            now.display()
        ));
    }
    if now != expected {
        return Err(format!("it now resolves to {}", now.display()));
    }
    let meta = std::fs::metadata(&now).map_err(|e| e.to_string())?;
    let right_kind = if dir { meta.is_dir() } else { meta.is_file() };
    if !right_kind {
        return Err(if dir {
            "not a directory"
        } else {
            "not a regular file"
        }
        .to_string());
    }
    Ok(())
}

/// Opens the manifest, then checks the file actually opened, not the path:
/// a check on the path followed by a second resolution at open time would let
/// a concurrent writer swap `SKILL.md` for a link in between. The body is read
/// from the returned descriptor only.
pub(super) fn open_in_place(path: &Path, roots: &[PathBuf]) -> Result<std::fs::File, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    let opened = opened_path(&file, path)?;
    if !roots.iter().any(|root| opened.starts_with(root)) {
        return Err(format!(
            "it resolved outside the skill roots, to {}",
            opened.display()
        ));
    }
    if opened != path {
        return Err(format!("it resolved to {}", opened.display()));
    }
    Ok(file)
}

/// The real path of an open file. On Linux the kernel reports it through
/// `/proc/self/fd`, which names the object behind the descriptor whatever
/// happened to the path since. Elsewhere the path is resolved again and the
/// file identity (device and inode) compared, which narrows the window to the
/// resolution itself.
fn opened_path(file: &std::fs::File, path: &Path) -> Result<PathBuf, String> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let _ = path;
        std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::unix::fs::MetadataExt;
        let now = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
        let (a, b) = (
            file.metadata().map_err(|e| e.to_string())?,
            std::fs::metadata(&now).map_err(|e| e.to_string())?,
        );
        if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
            return Err("the file changed while it was opened".to_string());
        }
        Ok(now)
    }
}

pub(super) fn load(skill: &Skill, roots: &[PathBuf]) -> ToolResult {
    let unreadable = |why: String| {
        ToolResult::error(
            "skill_unreadable",
            format!("cannot read {}: {why}", skill.path.display()),
        )
    };
    if let Err(why) = still_in_place(&skill.path, &skill.path, roots, false) {
        return unreadable(why);
    }
    let file = match open_in_place(&skill.path, roots) {
        Ok(file) => file,
        Err(why) => return unreadable(why),
    };
    let limit = frontmatter::MAX_HEADER_BYTES + MAX_BODY_BYTES;
    let mut bytes = Vec::new();
    if let Err(e) = file.take(limit as u64 + 1).read_to_end(&mut bytes) {
        return unreadable(e.to_string());
    }
    let text = String::from_utf8_lossy(&bytes);
    let body = match frontmatter::split_bounded(&text) {
        Ok((_, body)) => body.trim_matches('\n'),
        Err(why) => return unreadable(why),
    };
    if bytes.len() > limit || body.len() > MAX_BODY_BYTES {
        return ToolResult::error(
            "skill_oversized",
            format!(
                "the body of {} is larger than {MAX_BODY_BYTES} bytes; read the file in ranges with `read`",
                skill.path.display()
            ),
        );
    }
    if let Err(why) = still_in_place(&skill.base_dir, &skill.base_dir, roots, true) {
        return unreadable(why);
    }
    let mut resources = Vec::new();
    let mut entries = 0;
    list_resources(&skill.base_dir, "", 0, &mut entries, &mut resources);
    // The listing only names files (their content is read later through
    // `read`, which validates every path); checking the directory again after
    // the walk drops a listing taken from a directory swapped meanwhile.
    if let Err(why) = still_in_place(&skill.base_dir, &skill.base_dir, roots, true) {
        return unreadable(why);
    }
    let omitted = resources.len().saturating_sub(MAX_RESOURCES);
    resources.truncate(MAX_RESOURCES);

    let mut output = format!(
        "<skill name=\"{}\" path=\"{}\" base_dir=\"{}\">\n{body}\n</skill>",
        skill.name,
        skill.path.display(),
        skill.base_dir.display()
    );
    if !resources.is_empty() {
        output.push_str("\n\nFiles in the skill directory (relative to base_dir):");
        for r in &resources {
            output.push_str(&format!("\n- {r}"));
        }
        if omitted > 0 {
            output.push_str(&format!("\n[{omitted} more files omitted]"));
        }
    }
    ToolResult::ok(output).with_metadata(json!({
        "name": skill.name,
        "path": skill.path,
        "base_dir": skill.base_dir,
        "resources": resources,
        "resources_omitted": omitted,
    }))
}

/// The files below `dir`, relative and sorted, without `SKILL.md`, links,
/// nor the directories no skill walk enters. Stops at the entry bound.
pub(super) fn list_resources(
    dir: &Path,
    prefix: &str,
    depth: usize,
    entries: &mut usize,
    out: &mut Vec<String>,
) {
    if *entries >= MAX_RESOURCE_ENTRIES {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    // At most the budget left is taken from the iterator, so a huge
    // directory is neither collected nor sorted whole; the budget is shared
    // by the whole listing, which stops once it is spent.
    let remaining = MAX_RESOURCE_ENTRIES - *entries;
    let mut names: Vec<_> = read.filter_map(Result::ok).take(remaining).collect();
    *entries += names.len();
    names.sort_by_key(|e| e.file_name());
    for entry in names {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let rel = format!("{prefix}{name}");
        if kind.is_file() {
            if !(prefix.is_empty() && name == "SKILL.md") {
                out.push(rel);
            }
        } else if kind.is_dir() && depth < MAX_DEPTH && !SKIPPED_DIRS.contains(&name.as_str()) {
            list_resources(&entry.path(), &format!("{rel}/"), depth + 1, entries, out);
        }
    }
}
