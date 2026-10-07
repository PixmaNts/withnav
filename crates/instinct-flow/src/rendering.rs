//! Deterministic state and action rendering; placeholders never evaluate code.
use crate::{Diagnostic, InputValue};
use std::collections::{BTreeMap, BTreeSet};
fn limit() -> Diagnostic {
    Diagnostic::new("limit", "rendered value exceeds byte limit", "rendering")
}
fn append(out: &mut String, text: &str, cap: usize) -> Result<(), Diagnostic> {
    if text.len() > cap.saturating_sub(out.len()) {
        return Err(limit());
    }
    out.push_str(text);
    Ok(())
}
pub(crate) fn json_text(value: &serde_json::Value, cap: usize) -> Result<String, Diagnostic> {
    struct Writer {
        bytes: Vec<u8>,
        cap: usize,
    }
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("byte limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        bytes: Vec::new(),
        cap,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| limit())?;
    String::from_utf8(writer.bytes).map_err(|_| limit())
}
pub fn placeholders(text: &str, state: bool) -> Result<BTreeSet<String>, Diagnostic> {
    render(text, &BTreeMap::new(), state, true, 65536).map(|(_, names)| names)
}
fn render(
    text: &str,
    values: &BTreeMap<String, InputValue>,
    state: bool,
    admit: bool,
    cap: usize,
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
            append(&mut out, open, cap)?;
            rest = &rest[escape_open.len()..];
        } else if rest.starts_with(escape_close) {
            append(&mut out, close, cap)?;
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
                let rendered;
                let text = match &value.value {
                    Some(v) if !v.is_null() => {
                        if let Some(s) = v.as_str() {
                            s
                        } else {
                            rendered = json_text(v, cap.saturating_sub(out.len()))?;
                            &rendered
                        }
                    }
                    _ => {
                        if state {
                            "<absent>"
                        } else {
                            "—"
                        }
                    }
                };
                append(&mut out, text, cap)?;
                if state && value.cut {
                    append(&mut out, " [cut]", cap)?;
                }
            }
            rest = &tail[end + close.len()..];
        } else if rest.starts_with(close) || rest.starts_with('{') || rest.starts_with('}') {
            return Err(Diagnostic::new("template", "unescaped brace", "template"));
        } else {
            let ch = rest.chars().next().expect("nonempty renderer input");
            append(&mut out, ch.encode_utf8(&mut [0; 4]), cap)?;
            rest = &rest[ch.len_utf8()..];
        }
    }
    Ok((out, names))
}
pub fn state(
    values: &BTreeMap<String, InputValue>,
    template: Option<&str>,
) -> Result<String, Diagnostic> {
    state_bounded(values, template, 65536)
}
pub(crate) fn state_bounded(
    values: &BTreeMap<String, InputValue>,
    template: Option<&str>,
    cap: usize,
) -> Result<String, Diagnostic> {
    if let Some(template) = template {
        return render(template, values, true, false, cap).map(|(s, _)| s);
    }
    let mut out = String::new();
    for (alias, value) in values {
        if !out.is_empty() {
            append(&mut out, "\n", cap)?;
        }
        append(&mut out, alias, cap)?;
        append(&mut out, " = ", cap)?;
        if let Some(v) = value.value.as_ref().filter(|v| !v.is_null()) {
            let text = json_text(v, cap.saturating_sub(out.len()))?;
            append(&mut out, &text, cap)?;
        } else {
            append(&mut out, "<absent>", cap)?;
        }
        if value.cut {
            append(&mut out, " [cut]", cap)?;
        }
    }
    Ok(out)
}
pub fn params(
    params: &BTreeMap<String, serde_json::Value>,
    values: &BTreeMap<String, InputValue>,
) -> Result<BTreeMap<String, serde_json::Value>, Diagnostic> {
    params
        .iter()
        .map(|(name, value)| {
            let value = if let Some(text) = value.as_str() {
                serde_json::Value::String(render(text, values, false, false, 16384)?.0)
            } else if let Some(alias) = value
                .as_object()
                .and_then(|m| m.get("ref"))
                .and_then(|v| v.as_str())
            {
                if values.get(alias).is_some_and(|v| v.cut) {
                    return Err(limit());
                }
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
    fn repeated_placeholder_bomb_stops_at_append_ceiling() {
        let values = BTreeMap::from([(
            "x".into(),
            InputValue {
                value: Some("x".repeat(16384).into()),
                cut: false,
                redacted: false,
            },
        )]);
        let template = "{{x}}".repeat(12000);
        let start = std::time::Instant::now();
        assert_eq!(state(&values, Some(&template)).unwrap_err().code, "limit");
        assert_eq!(
            params(
                &BTreeMap::from([("text".into(), "{x}".repeat(5000).into())]),
                &values
            )
            .unwrap_err()
            .code,
            "limit"
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
    #[test]
    fn absent_and_cut_rendering_is_explicit() {
        let values = BTreeMap::from([
            (
                "x".into(),
                InputValue {
                    value: None,
                    cut: false,
                    redacted: false,
                },
            ),
            (
                "y".into(),
                InputValue {
                    value: Some("hello".into()),
                    cut: true,
                    redacted: false,
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
