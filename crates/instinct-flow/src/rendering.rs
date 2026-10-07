//! Deterministic state and action rendering; placeholders never evaluate code.
use crate::{Diagnostic, InputValue};
use std::collections::{BTreeMap, BTreeSet};
pub fn placeholders(text: &str, state: bool) -> Result<BTreeSet<String>, Diagnostic> {
    render(text, &BTreeMap::new(), state, true).map(|(_, names)| names)
}
fn render(
    text: &str,
    values: &BTreeMap<String, InputValue>,
    state: bool,
    admit: bool,
) -> Result<(String, BTreeSet<String>), Diagnostic> {
    let (open, close, escape_open, escape_close) = if state {
        ("{{", "}}", "{{{{", "}}}}")
    } else {
        ("{", "}", "{{", "}}")
    };
    let mut rest = text;
    let mut out = String::new();
    let mut names = BTreeSet::new();
    while !rest.is_empty() {
        if rest.starts_with(escape_open) {
            out.push_str(open);
            rest = &rest[escape_open.len()..];
        } else if rest.starts_with(escape_close) {
            out.push_str(close);
            rest = &rest[escape_close.len()..];
        } else if let Some(tail) = rest.strip_prefix(open) {
            let end = tail
                .find(close)
                .ok_or_else(|| Diagnostic::new("template", "unclosed placeholder", "template"))?;
            let name = &tail[..end];
            if !crate::expression::identifier(name) {
                return Err(Diagnostic::new(
                    "template",
                    "placeholder must name a local alias",
                    "template",
                ));
            }
            names.insert(name.to_string());
            if !admit {
                let value = values.get(name).ok_or_else(|| {
                    Diagnostic::new("template", "undeclared placeholder", "template")
                })?;
                out.push_str(&match &value.value {
                    Some(v) if !v.is_null() => v
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string()),
                    _ => {
                        if state {
                            "<absent>".into()
                        } else {
                            "—".into()
                        }
                    }
                });
                if state && value.cut {
                    out.push_str(" [cut]");
                }
            }
            rest = &tail[end + close.len()..];
        } else if rest.starts_with(close) || rest.starts_with('{') || rest.starts_with('}') {
            return Err(Diagnostic::new("template", "unescaped brace", "template"));
        } else {
            let ch = rest.chars().next().expect("nonempty renderer input");
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    Ok((out, names))
}
pub fn state(
    values: &BTreeMap<String, InputValue>,
    template: Option<&str>,
) -> Result<String, Diagnostic> {
    if let Some(template) = template {
        return render(template, values, true, false).map(|(s, _)| s);
    }
    Ok(values
        .iter()
        .map(|(alias, v)| {
            format!(
                "{alias} = {}{}",
                v.value
                    .as_ref()
                    .filter(|v| !v.is_null())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "<absent>".into()),
                if v.cut { " [cut]" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}
pub fn params(
    params: &BTreeMap<String, serde_json::Value>,
    values: &BTreeMap<String, InputValue>,
) -> Result<BTreeMap<String, serde_json::Value>, Diagnostic> {
    params
        .iter()
        .map(|(name, value)| {
            let value = if let Some(text) = value.as_str() {
                serde_json::Value::String(render(text, values, false, false)?.0)
            } else if let Some(alias) = value
                .as_object()
                .and_then(|m| m.get("ref"))
                .and_then(|v| v.as_str())
            {
                values
                    .get(alias)
                    .and_then(|v| v.value.clone())
                    .filter(|v| !v.is_null())
                    .ok_or_else(|| {
                        Diagnostic::new("invalid_parameter", "typed parameter is absent", name)
                    })?
            } else {
                value.clone()
            };
            if value.as_str().is_some_and(|s| s.len() > 16384) {
                return Err(Diagnostic::new(
                    "limit",
                    "action text exceeds byte limit",
                    name,
                ));
            }
            Ok((name.clone(), value))
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_and_cut_rendering_is_explicit() {
        let values = BTreeMap::from([
            (
                "x".into(),
                InputValue {
                    value: None,
                    cut: false,
                },
            ),
            (
                "y".into(),
                InputValue {
                    value: Some("hello".into()),
                    cut: true,
                },
            ),
        ]);
        assert_eq!(
            state(&values, None).unwrap(),
            "x = <absent>\ny = \"hello\" [cut]"
        );
        assert_eq!(
            state(&values, Some("{{{{literal}}}} {{x}} {{y}}")).unwrap(),
            "{{literal}} <absent> hello [cut]"
        );
        assert_eq!(
            params(
                &BTreeMap::from([("note".into(), "{{x}} {x}".into())]),
                &values
            )
            .unwrap()["note"],
            "{x} —"
        );
    }
}
