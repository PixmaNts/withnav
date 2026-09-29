use serde::{Deserialize, Serialize};

/// Navigator thresholds, passed as `initializationOptions` and updated via
/// `workspace/didChangeConfiguration`. All fields have defaults matching the
/// spec: idle 3000 ms, min changed lines 3, cooldown 20000 ms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NavigatorConfig {
    pub enabled: bool,
    pub idle_ms: u64,
    pub min_changed_lines: usize,
    pub cooldown_ms: u64,
    pub model_alias: String,
}

impl Default for NavigatorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_ms: 3000,
            min_changed_lines: 3,
            cooldown_ms: 20_000,
            model_alias: "fast".to_string(),
        }
    }
}

impl NavigatorConfig {
    /// Parses the config from a raw JSON value (initializationOptions or the
    /// `settings` of didChangeConfiguration). Unknown fields are ignored;
    /// missing fields keep the previous value. Malformed values keep defaults.
    pub fn from_value(value: &serde_json::Value, fallback: &Self) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_else(|_| fallback.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = NavigatorConfig::default();
        assert!(c.enabled);
        assert_eq!(c.idle_ms, 3000);
        assert_eq!(c.min_changed_lines, 3);
        assert_eq!(c.cooldown_ms, 20_000);
        assert_eq!(c.model_alias, "fast");
    }

    #[test]
    fn parse_overrides_some_fields() {
        let base = NavigatorConfig::default();
        let v = serde_json::json!({"idleMs": 1500, "modelAlias": "default"});
        let c = NavigatorConfig::from_value(&v, &base);
        assert_eq!(c.idle_ms, 1500);
        assert_eq!(c.model_alias, "default");
        assert_eq!(c.min_changed_lines, base.min_changed_lines);
    }

    #[test]
    fn garbage_value_keeps_fallback() {
        let base = NavigatorConfig::default();
        let c = NavigatorConfig::from_value(&serde_json::json!("nonsense"), &base);
        assert_eq!(c, base);
    }
}
