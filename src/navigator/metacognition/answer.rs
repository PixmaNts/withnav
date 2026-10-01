//! Parsing of the metacognition's reply: the prompt contract's JSON (a remark or
//! explicit silence) out of prose and code fences.

use crate::navigator::lsp::protocol::Remark;

/// What the metacognition's answer parsed to: a remark, explicit silence,
/// or output that violates the prompt contract (non-empty but unparseable).
/// Malformed output is not silence: the log records it as `malformed` so
/// prompt regressions are visible instead of silently eaten.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedAnswer {
    Remark(Remark),
    Silent,
    Malformed,
}

/// Parses the model's answer per the prompt contract: JSON only, either
/// `{"silent": true}` or `{"line": <1-based>, "text": "..."}`. Tolerates code
/// fences and surrounding prose; empty output counts as silence.
pub fn parse_answer(raw: &str) -> ParsedAnswer {
    if raw.trim().is_empty() {
        return ParsedAnswer::Silent;
    }
    let Some(json_str) = extract_json(raw) else {
        return ParsedAnswer::Malformed;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str.trim()) else {
        return ParsedAnswer::Malformed;
    };
    if v.get("silent").and_then(|s| s.as_bool()).unwrap_or(false) {
        return ParsedAnswer::Silent;
    }
    let Some(line) = v.get("line").and_then(|l| l.as_u64()) else {
        return ParsedAnswer::Malformed;
    };
    if line == 0 {
        return ParsedAnswer::Malformed;
    }
    let Some(text) = v.get("text").and_then(|t| t.as_str()) else {
        return ParsedAnswer::Malformed;
    };
    if text.trim().is_empty() {
        return ParsedAnswer::Malformed;
    }
    match u32::try_from(line - 1) {
        Ok(line0) => ParsedAnswer::Remark(Remark {
            // model speaks 1-based; wire speaks 0-based
            line: line0,
            text: text.trim().to_string(),
        }),
        Err(_) => ParsedAnswer::Malformed,
    }
}

/// Extracts the innermost balanced JSON object from prose / code fences.
fn extract_json(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    // Direct hit first.
    if trimmed.starts_with('{') && serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed);
    }
    // Otherwise find the first balanced { ... } and try it.
    let bytes = trimmed.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&trimmed[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_remark() {
        assert_eq!(
            parse_answer(r#"{"line": 12, "text": "careful, missing error path"}"#),
            ParsedAnswer::Remark(Remark {
                line: 11,
                text: "careful, missing error path".to_string()
            })
        );
    }

    #[test]
    fn parses_silent() {
        assert_eq!(parse_answer(r#"{"silent": true}"#), ParsedAnswer::Silent);
    }

    #[test]
    fn garbage_is_malformed_not_silence() {
        assert_eq!(
            parse_answer("I think you should refactor this"),
            ParsedAnswer::Malformed
        );
        assert_eq!(parse_answer(""), ParsedAnswer::Silent);
    }

    #[test]
    fn json_without_silent_or_line_is_malformed() {
        assert_eq!(
            parse_answer(r#"{"silent": false}"#),
            ParsedAnswer::Malformed
        );
        assert_eq!(parse_answer(r#"{"other": 1}"#), ParsedAnswer::Malformed);
    }

    #[test]
    fn parse_model_answer_folds_malformed_to_silence() {
        assert_eq!(parse_answer("nope"), ParsedAnswer::Malformed);
        assert_eq!(
            parse_answer(r#"{"line": 0, "text": "x"}"#),
            ParsedAnswer::Malformed
        );
        assert_eq!(
            parse_answer(r#"{"line": 1, "text": "  "}"#),
            ParsedAnswer::Malformed
        );
    }

    #[test]
    fn json_in_code_fences_is_parsed() {
        let raw = "```json\n{\"line\": 3, \"text\": \"off by one\"}\n```";
        assert_eq!(
            parse_answer(raw),
            ParsedAnswer::Remark(Remark {
                line: 2,
                text: "off by one".to_string()
            })
        );
    }

    #[test]
    fn json_wrapped_in_prose_is_parsed() {
        let raw = "Here is my answer:\n{\"line\": 7, \"text\": \"edge case\"}\nHope it helps";
        assert_eq!(
            parse_answer(raw),
            ParsedAnswer::Remark(Remark {
                line: 6,
                text: "edge case".to_string()
            })
        );
    }
}
