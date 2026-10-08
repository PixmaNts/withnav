//! Read/write access to `.withnav/intent.md`: the workspace's intent
//! record (sections Intent / Plan / Decisions / Active spec). The Captain
//! may edit it by hand; the cognition updates it through the `intent`
//! tool when the conversation settles an intent, a plan or a decision.
//! When the sections are missing the file is treated as opaque text:
//! whatever is there is better than nothing.

use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{Tool, ToolContext, ToolResult};

/// The parsed intent record.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IntentRecord {
    /// `## Intent`: what the Captain is trying to do.
    pub intent: Option<String>,
    /// `## Plan`: the current plan.
    pub plan: Option<String>,
    /// `## Decisions`: settled decisions.
    pub decisions: Option<String>,
    /// `## Active spec`: a link to the spec being worked on.
    pub active_spec: Option<String>,
    /// The whole file, for prompt rendering (`{{intent}}` template).
    pub raw: String,
}

impl IntentRecord {
    /// True when at least one recognized section was found.
    pub fn has_sections(&self) -> bool {
        self.intent.is_some()
            || self.plan.is_some()
            || self.decisions.is_some()
            || self.active_spec.is_some()
    }

    /// The one-line-ish summary for templates: the Intent section when
    /// present, else the raw text.
    pub fn summary(&self) -> &str {
        self.intent.as_deref().unwrap_or(&self.raw)
    }
}

/// Reads `.withnav/intent.md` from the workspace, if present. Missing
/// file -> `Ok(None)`; present but unreadable -> the error (the caller
/// decides whether that is fatal; it never is for the editor).
pub fn read_intent(workspace: &Path) -> std::io::Result<Option<IntentRecord>> {
    let path = crate::paths::workspace_dir(workspace).join("intent.md");
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(Some(parse_intent(&raw)))
}

/// Parses the intent record: Markdown `## <Section>` headings with the
/// known names; anything else keeps the whole file as `raw`.
pub fn parse_intent(raw: &str) -> IntentRecord {
    let mut record = IntentRecord {
        raw: raw.trim().to_string(),
        ..Default::default()
    };
    let mut current: Option<&mut Option<String>> = None;
    for line in raw.lines() {
        let heading = line.strip_prefix("## ").map(str::trim);
        match heading {
            Some("Intent") => current = Some(&mut record.intent),
            Some("Plan") => current = Some(&mut record.plan),
            Some("Decisions") => current = Some(&mut record.decisions),
            Some("Active spec") => current = Some(&mut record.active_spec),
            Some(_) => current = None,
            None => {
                if let Some(section) = current.as_deref_mut() {
                    let section = section.get_or_insert_with(String::new);
                    if !section.is_empty() {
                        section.push('\n');
                    }
                    section.push_str(line);
                }
            }
        }
    }
    for field in [
        &mut record.intent,
        &mut record.plan,
        &mut record.decisions,
        &mut record.active_spec,
    ] {
        if let Some(text) = field {
            *text = text.trim().to_string();
            if text.is_empty() {
                *field = None;
            }
        }
    }
    record
}

/// Writes `.withnav/intent.md` from the workspace, replacing only the
/// sections given (the others keep their current text). The file is
/// created with all four headings when missing, and the write is atomic
/// (temp file + rename in the same directory). Sections set to an empty
/// string are cleared. A record that exists but cannot be read is an error
/// and is never overwritten.
pub fn write_intent_sections(
    workspace: &Path,
    sections: &IntentSections,
) -> std::io::Result<PathBuf> {
    let dir = workspace.join(".withnav");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("intent.md");
    // Only a missing record starts empty. Any other failure (permissions,
    // invalid UTF-8, a directory in the way) leaves the file as it is:
    // rebuilding from nothing would erase what the Captain wrote.
    let current = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let updated = apply_sections(&current, sections);
    // Atomic: a temp file in the SAME directory, fsynced, then renamed over
    // the target, so the Captain never sees a half-written record (the file
    // is version-controlled and may be open in the editor).
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    tmp.write_all(updated.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(path)
}

/// The sections a write may replace; `None` keeps the current text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntentSections {
    pub intent: Option<String>,
    pub plan: Option<String>,
    pub decisions: Option<String>,
    pub active_spec: Option<String>,
}

impl IntentSections {
    fn from_args(args: &Value) -> Self {
        let take = |key: &str| {
            args.get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
        };
        Self {
            intent: take("intent"),
            plan: take("plan"),
            decisions: take("decisions"),
            active_spec: take("active_spec"),
        }
    }

    fn is_empty(&self) -> bool {
        self.intent.is_none()
            && self.plan.is_none()
            && self.decisions.is_none()
            && self.active_spec.is_none()
    }
}

/// The four headings, in file order.
const SECTIONS: [&str; 4] = ["Intent", "Plan", "Decisions", "Active spec"];

/// Rebuilds the record: the four headings in order, each carrying its new
/// text when replaced, else its current text; any other content the file
/// carried is dropped (the record is machine-writable from the moment the
/// cognition first updates it).
fn apply_sections(current: &str, sections: &IntentSections) -> String {
    let record = parse_intent(current);
    let new_texts = [
        sections.intent.clone(),
        sections.plan.clone(),
        sections.decisions.clone(),
        sections.active_spec.clone(),
    ];
    let current_texts = [
        record.intent,
        record.plan,
        record.decisions,
        record.active_spec,
    ];
    let mut out = String::from("# Intent record\n\n");
    for (heading, (new_text, current_text)) in SECTIONS
        .into_iter()
        .zip(new_texts.into_iter().zip(current_texts))
    {
        let text = new_text.or(current_text).unwrap_or_default();
        out.push_str(&format!("## {heading}\n"));
        if !text.is_empty() {
            out.push_str(&text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

/// The cognition's `intent` tool: replaces sections of the workspace's
/// intent record. Registered only in editor sessions, so a
/// plain session keeps the default toolset. Restricted to the one path:
/// it never takes a path argument.
pub struct IntentTool;

impl Tool for IntentTool {
    fn name(&self) -> &str {
        "intent"
    }

    fn schema(&self) -> Value {
        json!({
            "description": "Update the workspace intent record (.withnav/intent.md). Each given \
                section replaces its current text; omitted sections are kept. Update it briefly, \
                without asking, when the conversation settles an intent, a plan or a decision.",
            "parameters": {
                "type": "object",
                "properties": {
                    "intent": {"type": "string", "description": "What the Captain is trying to do"},
                    "plan": {"type": "string", "description": "The current plan"},
                    "decisions": {"type": "string", "description": "Settled decisions"},
                    "active_spec": {"type": "string", "description": "Link to the spec being worked on"}
                },
                "additionalProperties": false
            }
        })
    }

    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        let sections = IntentSections::from_args(&args);
        Box::pin(async move {
            if sections.is_empty() {
                return Ok(ToolResult::error(
                    "invalid_args",
                    "no section to update: give at least one of intent, plan, decisions, active_spec",
                ));
            }
            // The tool context's workspace is the session's workspace: the
            // same root the editor and the other layers resolve the record
            // from. There is no path argument to abuse.
            match write_intent_sections(&cx.workspace, &sections) {
                Ok(path) => Ok(ToolResult::ok(format!(
                    "intent record updated: {}",
                    path.display()
                ))),
                Err(e) => Ok(ToolResult::error("io_error", e.to_string())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Intent record

## Intent
Add retry with backoff to the HTTP client.

## Plan
- read the current client
- add a retry wrapper

## Decisions
- exponential backoff, capped at 5 attempts

## Active spec
.scratch/navigator/spec.md
";

    #[test]
    fn parses_all_four_sections() {
        let r = parse_intent(SAMPLE);
        assert_eq!(
            r.intent.as_deref(),
            Some("Add retry with backoff to the HTTP client.")
        );
        assert!(r.plan.as_deref().unwrap().contains("retry wrapper"));
        assert!(r.decisions.as_deref().unwrap().contains("exponential"));
        assert_eq!(r.active_spec.as_deref(), Some(".scratch/navigator/spec.md"));
        assert!(r.has_sections());
        assert_eq!(r.summary(), "Add retry with backoff to the HTTP client.");
    }

    #[test]
    fn missing_sections_fall_back_to_raw() {
        let r = parse_intent("just some notes\nabout what I am doing");
        assert!(!r.has_sections());
        assert_eq!(r.summary(), "just some notes\nabout what I am doing");
    }

    #[test]
    fn unknown_headings_do_not_land_in_sections() {
        let r = parse_intent("## Intent\ndo the thing\n## Other\nnot captured");
        assert_eq!(r.intent.as_deref(), Some("do the thing"));
        assert!(r.plan.is_none());
    }

    #[test]
    fn reads_from_workspace_navigator_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_intent(tmp.path()).unwrap().is_none());
        let dir = tmp.path().join(".withnav");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("intent.md"), "## Intent\nx").unwrap();
        let r = read_intent(tmp.path()).unwrap().unwrap();
        assert_eq!(r.intent.as_deref(), Some("x"));
    }

    #[test]
    fn empty_sections_are_none() {
        let r = parse_intent("## Intent\n\n## Plan\nreal plan");
        assert_eq!(r.intent, None);
        assert_eq!(r.plan.as_deref(), Some("real plan"));
    }

    fn tool_context(root: &Path) -> crate::tools::ToolContext {
        crate::tools::ToolContext {
            binding: None,
            workspace: root.to_path_buf(),
            writable_roots: vec![root.to_path_buf()],
            readable_roots: Vec::new(),
        }
    }

    #[test]
    fn write_replaces_given_sections_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".withnav");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("intent.md"), SAMPLE).unwrap();

        write_intent_sections(
            tmp.path(),
            &IntentSections {
                intent: Some("add an `auth remove` command".into()),
                decisions: None,
                plan: None,
                active_spec: None,
            },
        )
        .unwrap();
        let r = read_intent(tmp.path()).unwrap().unwrap();
        assert_eq!(
            r.intent.as_deref(),
            Some("add an `auth remove` command"),
            "the given section is replaced"
        );
        assert!(r.plan.as_deref().unwrap().contains("retry wrapper"), "kept");
        assert!(r.decisions.as_deref().unwrap().contains("exponential"));
        assert_eq!(r.active_spec.as_deref(), Some(".scratch/navigator/spec.md"));
    }

    #[test]
    fn write_creates_the_record_with_all_headings_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        write_intent_sections(
            tmp.path(),
            &IntentSections {
                intent: Some("ship the navigator".into()),
                plan: None,
                decisions: None,
                active_spec: Some(".scratch/navigator/spec.md".into()),
            },
        )
        .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(".withnav/intent.md")).unwrap();
        for heading in [
            "# Intent record",
            "## Intent",
            "## Plan",
            "## Decisions",
            "## Active spec",
        ] {
            assert!(
                raw.contains(heading),
                "missing heading {heading} in:\n{raw}"
            );
        }
        let r = parse_intent(&raw);
        assert_eq!(r.intent.as_deref(), Some("ship the navigator"));
        assert_eq!(r.plan, None);
        assert_eq!(r.active_spec.as_deref(), Some(".scratch/navigator/spec.md"));
    }

    #[test]
    fn an_unreadable_record_is_an_error_and_stays_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".withnav");
        std::fs::create_dir_all(&dir).unwrap();
        // Not valid UTF-8: the read fails, the bytes must survive.
        let raw: &[u8] = b"## Intent\nkeep me \xff\xfe\n";
        std::fs::write(dir.join("intent.md"), raw).unwrap();

        let result = write_intent_sections(
            tmp.path(),
            &IntentSections {
                intent: Some("overwrite".into()),
                ..IntentSections::default()
            },
        );
        assert!(
            result.is_err(),
            "an unreadable record must not be rewritten"
        );
        assert_eq!(std::fs::read(dir.join("intent.md")).unwrap(), raw);
    }

    #[test]
    fn an_empty_section_clears_its_text() {
        let tmp = tempfile::tempdir().unwrap();
        write_intent_sections(
            tmp.path(),
            &IntentSections {
                intent: Some("kept".into()),
                plan: Some("obsolete plan".into()),
                decisions: None,
                active_spec: None,
            },
        )
        .unwrap();
        write_intent_sections(
            tmp.path(),
            &IntentSections {
                intent: None,
                plan: Some("".into()),
                decisions: None,
                active_spec: None,
            },
        )
        .unwrap();
        let r = read_intent(tmp.path()).unwrap().unwrap();
        assert_eq!(r.intent.as_deref(), Some("kept"));
        assert_eq!(r.plan, None, "an empty string clears the existing section");
    }

    #[tokio::test]
    async fn intent_tool_updates_the_workspace_record() {
        let tmp = tempfile::tempdir().unwrap();
        let cx = tool_context(tmp.path());
        let result = IntentTool
            .call(
                json!({"intent": "add an `auth remove` command", "decisions": "CLI subcommand, not a flag"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(result.status, crate::tools::ToolStatus::Ok, "{result:?}");
        let r = read_intent(tmp.path()).unwrap().unwrap();
        assert_eq!(r.intent.as_deref(), Some("add an `auth remove` command"));
        assert!(r.decisions.as_deref().unwrap().contains("CLI subcommand"));

        // A call with no section is a structured tool error, not a panic.
        let empty = IntentTool
            .call(json!({}), CancellationToken::new(), &cx)
            .await
            .unwrap();
        assert_eq!(empty.status, crate::tools::ToolStatus::Error);
        assert_eq!(empty.error.unwrap().code, "invalid_args");
    }
}
