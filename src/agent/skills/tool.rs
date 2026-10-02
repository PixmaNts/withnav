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
use std::path::Path;
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
const MAX_RESOURCE_ENTRIES: usize = 5_000;

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
            Ok(tokio::task::spawn_blocking(move || load(&skill))
                .await
                .unwrap_or_else(|e| ToolResult::error("skill_unreadable", e.to_string())))
        })
    }
}

fn load(skill: &Skill) -> ToolResult {
    let unreadable = |why: String| {
        ToolResult::error(
            "skill_unreadable",
            format!("cannot read {}: {why}", skill.path.display()),
        )
    };
    let limit = frontmatter::MAX_HEADER_BYTES + MAX_BODY_BYTES;
    let mut bytes = Vec::new();
    let read = std::fs::File::open(&skill.path)
        .and_then(|f| f.take(limit as u64 + 1).read_to_end(&mut bytes));
    if let Err(e) = read {
        return unreadable(e.to_string());
    }
    let text = String::from_utf8_lossy(&bytes);
    let body = match frontmatter::split(&text) {
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
    let mut resources = Vec::new();
    let mut entries = 0;
    list_resources(&skill.base_dir, "", 0, &mut entries, &mut resources);
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
fn list_resources(
    dir: &Path,
    prefix: &str,
    depth: usize,
    entries: &mut usize,
    out: &mut Vec<String>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<_> = read.filter_map(Result::ok).collect();
    names.sort_by_key(|e| e.file_name());
    for entry in names {
        *entries += 1;
        if *entries > MAX_RESOURCE_ENTRIES {
            return;
        }
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
