//! Read-only access to `.navigator/intent.md`: the workspace's intent
//! record (sections Intent / Plan / Decisions / Active spec). The driver
//! may edit it by hand; layers only read it here (writing from the
//! cognition is a later slice). When the sections are missing the file is
//! treated as opaque text: whatever is there is better than nothing.

use std::path::Path;

/// The parsed intent record.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IntentRecord {
    /// `## Intent`: what the driver is trying to do.
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

/// Reads `.navigator/intent.md` from the workspace, if present. Missing
/// file -> `Ok(None)`; present but unreadable -> the error (the caller
/// decides whether that is fatal; it never is for the editor).
pub fn read_intent(workspace: &Path) -> std::io::Result<Option<IntentRecord>> {
    let path = workspace.join(".navigator").join("intent.md");
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
        let dir = tmp.path().join(".navigator");
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
}
