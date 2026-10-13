//! Redact server text and structured values, and streaming bytes before ring eviction
//! (retaining incomplete secret prefixes).
use std::collections::VecDeque;

use serde_json::Value;

/// Every representation of `secret` a server can emit in text: the raw value, the value as
/// a JSON string body (newline, quote and backslash escaped), and the same with non-ASCII
/// characters `\u`-escaped, as Python's `json.dumps` writes by default.
fn forms(secret: &str) -> Vec<String> {
    let escaped = serde_json::to_string(secret).unwrap_or_default();
    let escaped = escaped
        .strip_prefix('"')
        .and_then(|e| e.strip_suffix('"'))
        .unwrap_or_default()
        .to_string();
    let mut ascii = String::new();
    for c in escaped.chars() {
        if c.is_ascii() {
            ascii.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                ascii.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    // JSON escapes are case-insensitive hexadecimal; cover both casings.
    let upper = upper_hex(&ascii);
    let mut forms = vec![secret.to_owned(), escaped, ascii, upper];
    forms.retain(|form| !form.is_empty());
    forms
}

fn upper_hex(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("\\u") {
        out.push_str(&rest[..start + 2]);
        let hex: String = rest[start + 2..].chars().take(4).collect();
        out.push_str(&hex.to_uppercase());
        rest = &rest[start + 2 + hex.len()..];
    }
    out.push_str(rest);
    out
}

pub(super) fn redact(text: &str, secrets: &[String]) -> String {
    let mut forms: Vec<String> = secrets
        .iter()
        .filter(|s| !s.is_empty())
        .flat_map(|s| forms(s))
        .collect();
    // Longest first, so a shorter secret never splits a longer one's escaped form.
    forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
    forms.dedup();
    let mut text = text.to_owned();
    for form in forms {
        text = text.replace(&form, "[redacted]");
    }
    text
}

/// Redacts every string and object key of a JSON value before it is serialised, where a
/// secret is still in its raw form.
pub(super) fn redact_value(value: &Value, secrets: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text, secrets)),
        Value::Array(items) => Value::Array(items.iter().map(|v| redact_value(v, secrets)).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(k, v)| (redact(k, secrets), redact_value(v, secrets)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

#[derive(Default)]
pub(super) struct StderrTail {
    pending: Vec<u8>,
    bytes: VecDeque<u8>,
}

impl StderrTail {
    pub fn push(&mut self, bytes: &[u8], secrets: &[String], eof: bool) {
        self.pending.extend_from_slice(bytes);
        let mut offset = 0;
        let mut safe = vec![];
        while offset < self.pending.len() {
            let rest = &self.pending[offset..];
            if !eof
                && secrets
                    .iter()
                    .any(|s| rest.len() < s.len() && s.as_bytes().starts_with(rest))
            {
                break;
            } else if let Some(secret) = secrets
                .iter()
                .find(|s| !s.is_empty() && rest.starts_with(s.as_bytes()))
            {
                safe.extend_from_slice(b"[redacted]");
                offset += secret.len();
            } else if !eof
                && secrets
                    .iter()
                    .any(|s| !s.is_empty() && s.as_bytes().starts_with(rest))
            {
                break;
            } else if eof
                && secrets
                    .iter()
                    .any(|s| !s.is_empty() && s.as_bytes().starts_with(rest))
            {
                safe.extend_from_slice(b"[redacted]");
                offset = self.pending.len();
            } else {
                safe.push(rest[0]);
                offset += 1;
            }
        }
        self.pending.drain(..offset);
        self.bytes.extend(safe);
        while self.bytes.len() > 64 * 1024 {
            self.bytes.pop_front();
        }
        let mut lines = self.bytes.iter().filter(|b| **b == b'\n').count();
        while lines > 200 {
            if self.bytes.pop_front() == Some(b'\n') {
                lines -= 1;
            }
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.iter().copied().collect::<Vec<_>>()).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_escaped_and_structured_forms_are_redacted() {
        for secret in ["multi\nline-sentinel", "quote\"sentinel", "back\\slash-sentinel", "unicodé-sentinel"] {
            let secrets = vec![secret.to_string()];
            let structured = json!({"k": secret, secret: [secret]});
            let redacted = serde_json::to_string(&redact_value(&structured, &secrets)).unwrap();
            assert!(!redacted.contains("sentinel"), "{redacted}");
            for text in [
                serde_json::to_string(&json!({"k": secret})).unwrap(),
                serde_json::to_string_pretty(&structured).unwrap(),
                format!("raw {secret} raw"),
            ] {
                assert!(!redact(&text, &secrets).contains("sentinel"), "{text}");
            }
        }
        let python = r#"{"token": "unicod\u00e9-sentinel"}"#;
        assert!(!redact(python, &["unicodé-sentinel".into()]).contains("sentinel"));
        let upper = r#"{"token": "unicod\u00E9-sentinel"}"#;
        assert!(!redact(upper, &["unicodé-sentinel".into()]).contains("sentinel"));
    }
    #[test]
    fn secrets_split_at_read_and_eviction_boundaries_never_enter_tail() {
        let secrets = vec!["abcdefgh".into()];
        let mut tail = StderrTail::default();
        tail.push(&vec![b'x'; 65534], &secrets, false);
        tail.push(b"abc", &secrets, false);
        assert!(!tail.text().ends_with("abc"));
        tail.push(b"defgh retained", &secrets, false);
        assert!(tail.text().ends_with("[redacted] retained"));
        assert!(!tail.text().contains("abcdefgh"));
        tail.push(b"abc", &secrets, true);
        assert!(tail.text().ends_with("[redacted]"));
    }
}
