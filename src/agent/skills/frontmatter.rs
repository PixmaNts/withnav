//! The YAML header of a `SKILL.md`: `name` and `description` are required,
//! `disable-model-invocation: true` keeps the skill out of the catalog, and
//! every other key (version, author, license, platforms, metadata…) is
//! tolerated and ignored.

use serde::Deserialize;

/// The most bytes read to find the end of the header.
pub(super) const MAX_HEADER_BYTES: usize = 16 * 1024;
/// The longest accepted id.
const MAX_NAME_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Meta {
    pub name: String,
    pub description: String,
    pub disabled: bool,
}

#[derive(Deserialize)]
struct Raw {
    name: Option<String>,
    description: Option<String>,
    #[serde(default, rename = "disable-model-invocation")]
    disabled: Option<bool>,
}

/// Splits `text` into the YAML header and the body that follows it.
pub(crate) fn split(text: &str) -> Result<(&str, &str), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return Err("no YAML header (the file must start with ---)".into());
    }
    let start = text.find('\n').map_or(text.len(), |i| i + 1);
    let mut at = start;
    for line in lines {
        if matches!(line.trim_end(), "---" | "...") {
            return Ok((&text[start..at], &text[at + line.len()..]));
        }
        at += line.len();
    }
    Err("the YAML header is not closed".into())
}

/// Parses the header of a whole file.
pub(super) fn parse(text: &str) -> Result<(Meta, &str), String> {
    let (yaml, body) = split_bounded(text)?;
    let raw: Raw = serde_saphyr::from_str(yaml).map_err(|e| format!("invalid YAML header: {e}"))?;
    let name = raw.name.map(|n| n.trim().to_string()).unwrap_or_default();
    if name.is_empty() {
        return Err("`name` is required".into());
    }
    if name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "`name` must be at most {MAX_NAME_CHARS} characters, without spaces"
        ));
    }
    let description = raw
        .description
        .map(|d| d.trim().to_string())
        .unwrap_or_default();
    if description.is_empty() {
        return Err("`description` is required".into());
    }
    Ok((
        Meta {
            name,
            description,
            disabled: raw.disabled.unwrap_or(false),
        },
        body,
    ))
}

/// Skills use a bounded header during discovery and when loading the full body.
pub(super) fn split_bounded(text: &str) -> Result<(&str, &str), String> {
    let (yaml, body) = split(text)?;
    if text.strip_prefix('\u{feff}').unwrap_or(text).len() - body.len() > MAX_HEADER_BYTES {
        return Err(format!(
            "the YAML header is over the {MAX_HEADER_BYTES} byte cap"
        ));
    }
    Ok((yaml, body))
}
