use serde::{Deserialize, Serialize};

/// Navigator settings, passed as `initializationOptions` and updated via
/// `workspace/didChangeConfiguration`. The pre-filter thresholds and the
/// instinct's model live in the decision pipeline; keys the editor still
/// sends for them are ignored, never rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NavigatorConfig {
    pub enabled: bool,
    pub idle_ms: u64,
}

impl Default for NavigatorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_ms: 3000,
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
    }

    #[test]
    fn parse_overrides_some_fields() {
        let base = NavigatorConfig::default();
        let v = serde_json::json!({"idleMs": 1500});
        let c = NavigatorConfig::from_value(&v, &base);
        assert_eq!(c.idle_ms, 1500);
        assert!(c.enabled);
    }

    #[test]
    fn keys_the_pipeline_now_owns_still_parse() {
        let base = NavigatorConfig::default();
        let v = serde_json::json!({
            "idleMs": 1500,
            "minChangedLines": 1,
            "cooldownMs": 60000,
            "modelAlias": "default",
        });
        let c = NavigatorConfig::from_value(&v, &base);
        assert_eq!(c.idle_ms, 1500, "the rest of the object is not discarded");
    }

    #[test]
    fn garbage_value_keeps_fallback() {
        let base = NavigatorConfig::default();
        let c = NavigatorConfig::from_value(&serde_json::json!("nonsense"), &base);
        assert_eq!(c, base);
    }
}
