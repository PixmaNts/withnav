//! `{{var}}` template substitution: substitution only, no logic. The
//! variable list is fixed by the spec: `file`, `language`, `intent`,
//! `diff`, `context`, `diagnostics`, `live_state`, `conversation`. Unknown
//! `{{name}}` fails validation.

use super::PipelineError;

/// The template variables, in spec order.
pub const TEMPLATE_VARIABLES: [&str; 8] = [
    "file",
    "language",
    "intent",
    "diff",
    "context",
    "diagnostics",
    "live_state",
    "conversation",
];

/// Validates that a template only references known variables.
pub(crate) fn validate_template(tmpl: &str, source: &'static str) -> Result<(), PipelineError> {
    if let Some(name) = unknown_names(tmpl).first() {
        return Err(PipelineError::invalid(
            source,
            format!("template uses unknown variable `{{{{{name}}}}}`"),
        ));
    }
    Ok(())
}

/// Renders a template, substituting `{{name}}` with the map's value. A
/// missing value renders as the empty string (validation has already
/// rejected unknown names; a known name without a value means "nothing to
/// say", e.g. no intent recorded).
pub(crate) fn render(tmpl: &str, vars: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(tmpl.len());
    let mut rest = tmpl;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = &after[..end];
                if let Some(v) = vars.get(name) {
                    out.push_str(v);
                }
                rest = &after[end + 2..];
            }
            None => {
                // Unterminated `{{`: keep it literally.
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn unknown_names(tmpl: &str) -> Vec<&str> {
    let mut unknown = Vec::new();
    let mut rest = tmpl;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = &after[..end];
                if !TEMPLATE_VARIABLES.contains(&name) {
                    unknown.push(name);
                }
                rest = &after[end + 2..];
            }
            None => break,
        }
    }
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            ("file".to_string(), "src/main.rs".to_string()),
            ("language".to_string(), "rust".to_string()),
            ("intent".to_string(), "add retry".to_string()),
            ("diff".to_string(), "+ x".to_string()),
            ("context".to_string(), "1: fn a()".to_string()),
            ("diagnostics".to_string(), String::new()),
            ("live_state".to_string(), String::new()),
            ("conversation".to_string(), String::new()),
        ])
    }

    #[test]
    fn substitutes_known_variables() {
        let out = render("File {{file}} ({{language}}):\n{{diff}}", &vars());
        assert_eq!(out, "File src/main.rs (rust):\n+ x");
    }

    #[test]
    fn missing_value_renders_empty() {
        let mut v = vars();
        v.remove("intent");
        assert_eq!(render("[{{intent}}]", &v), "[]");
    }

    #[test]
    fn unterminated_braces_kept_literally() {
        assert_eq!(render("a {{ b", &vars()), "a {{ b");
    }

    #[test]
    fn validate_accepts_all_known() {
        let all = TEMPLATE_VARIABLES
            .iter()
            .fold(String::new(), |acc, v| acc + "{{" + v + "}}");
        assert!(validate_template(&all, "test").is_ok());
    }

    #[test]
    fn validate_rejects_unknown_name() {
        let err = validate_template("{{nope}}", "test").unwrap_err();
        assert!(
            err.to_string().contains("unknown variable `{{nope}}`"),
            "{err}"
        );
    }

    #[test]
    fn repeated_variables_all_substituted() {
        assert_eq!(
            render("{{file}}={{file}}", &vars()),
            "src/main.rs=src/main.rs"
        );
    }
}
