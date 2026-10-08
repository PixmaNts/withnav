//! Catalogue-aware field policy owned by the settings HTTP adapter.
use crate::agent::definitions::Agents;
use crate::provider::{
    assignments::{AssignmentResolver, Layer},
    config::{ApiKind, ModelsConfig},
};
use crate::settings::{AssignmentVariant, ModelAssignment, Settings};
use serde_json::{json, Value};
use std::path::Path;

pub fn keys(agents: &Agents) -> Vec<String> {
    let mut keys: Vec<String> = [
        "layers.reflex",
        "layers.instinct",
        "layers.metacognition",
        "default_agent",
        "permissions.automatic_review",
        "permissions.scratch_dirs",
        "subagents.max_concurrent",
        "subagents.watchdog_interval_secs",
        "agent.watchdog_interval_secs",
        "tui.theme",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    keys.extend(agents.all().map(|a| format!("agents.{}", a.id)));
    keys
}

pub fn default_value(key: &str) -> Value {
    match key {
        "default_agent" => json!("build"),
        "permissions.automatic_review" => json!(true),
        "permissions.scratch_dirs" => json!(Settings::default()
            .permissions_scratch_dirs()
            .unwrap_or_default()),
        "subagents.max_concurrent" => json!(4),
        "subagents.watchdog_interval_secs" | "agent.watchdog_interval_secs" => json!(300),
        _ => Value::Null,
    }
}

pub fn applies(key: &str) -> &'static str {
    if key == "tui.theme" {
        "live"
    } else if key.starts_with("layers.") {
        "restart"
    } else {
        "new_sessions"
    }
}

pub fn label(key: &str) -> &str {
    match key {
        "layers.reflex" => "Reflex · Completion",
        "layers.instinct" => "Instinct · Triage",
        "layers.metacognition" => "Metacognition · Beacon",
        "default_agent" => "Cognition · Nav and the crew",
        _ => key,
    }
}

pub fn description(key: &str) -> &'static str {
    match key {
        "layers.reflex" => "Completes as you type from code around the cursor.",
        "layers.instinct" => "Triages events and runs permission review.",
        "layers.metacognition" => {
            "Compares code with intent, writes Beacons signed by Nav, and wakes Nav on drift."
        }
        "default_agent" => {
            "Selects the primary role for Nav and the crew, with their tools, skills and rules."
        }
        _ => "",
    }
}

pub fn choice(raw: &Value) -> Value {
    if let Some(model) = raw.as_str() {
        json!({"model":model,"variant":null})
    } else if raw.get("model").is_some() {
        let variant = match raw.get("variant") {
            Some(Value::Null) => json!("off"),
            Some(v) => v.clone(),
            None => Value::Null,
        };
        json!({"model":raw["model"],"variant":variant})
    } else {
        raw.clone()
    }
}

pub fn apply(settings: &mut Settings, key: &str, mut value: Value) {
    if (key.starts_with("agents.") || key.starts_with("layers.")) && value.is_object() {
        if let Some(Value::Object(existing)) = settings.get_dotted(key) {
            let mut merged = existing.clone();
            merged.insert("model".into(), value["model"].take());
            if let Some(variant) = value.get("variant") {
                merged.insert("variant".into(), variant.clone());
            } else {
                merged.remove("variant");
            }
            value = Value::Object(merged);
        }
    }
    settings.set_dotted(key, value);
}

pub fn validate(
    key: &str,
    value: &Value,
    catalog: &ModelsConfig,
    agents: &Agents,
    tiers: &[String],
) -> Result<Value, String> {
    if !keys(agents).iter().any(|k| k == key) {
        return Err(format!("unknown key {key}"));
    }
    if value.is_null() {
        return Ok(Value::Null);
    }
    let invalid = || format!("invalid value for {key}");
    match key {
        "permissions.automatic_review" if !value.is_boolean() => return Err(invalid()),
        "permissions.scratch_dirs"
            if !value.as_array().is_some_and(|a| {
                a.iter()
                    .all(|v| v.as_str().is_some_and(|p| Path::new(p).is_absolute()))
            }) =>
        {
            return Err(invalid())
        }
        "subagents.max_concurrent" if !value.as_u64().is_some_and(|n| (1..=64).contains(&n)) => {
            return Err(invalid())
        }
        "subagents.watchdog_interval_secs" | "agent.watchdog_interval_secs"
            if !value.as_u64().is_some_and(|n| n <= 31536000) =>
        {
            return Err(invalid())
        }
        "tui.theme"
            if !value.as_str().is_some_and(|v| {
                ["ayu-dark", "ayu-light", "nord-dark", "nord-light"].contains(&v)
            }) =>
        {
            return Err(invalid())
        }
        "default_agent"
            if !agents
                .all()
                .any(|a| Some(a.id.as_str()) == value.as_str() && a.mode.is_primary()) =>
        {
            return Err(invalid())
        }
        _ => {}
    }
    if key.starts_with("layers.") || key.starts_with("agents.") {
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(invalid)?;
        let variant = match value.get("variant") {
            None | Some(Value::Null) => AssignmentVariant::Default,
            Some(Value::String(v)) if v == "off" => AssignmentVariant::Off,
            Some(Value::String(v)) if !v.is_empty() => AssignmentVariant::Named(v.clone()),
            _ => return Err(invalid()),
        };
        let api = match key {
            "layers.reflex" => Layer::Reflex.required_api(),
            "layers.instinct" => Layer::Instinct.required_api(),
            _ => ApiKind::Chat,
        };
        AssignmentResolver::new(catalog, &Settings::default()).strict_assignment(
            &ModelAssignment {
                model: model.into(),
                variant,
            },
            api,
            tiers,
        )?;
        let mut result = json!({"model":model});
        if let Some(v) = value.get("variant").filter(|v| !v.is_null()) {
            result["variant"] = v.clone();
        }
        return Ok(result);
    }
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn applies_labels_remain_compatible() {
        assert_eq!(applies("layers.reflex"), "restart");
        assert_eq!(applies("agents.build"), "new_sessions");
        assert_eq!(applies("subagents.max_concurrent"), "new_sessions");
        assert_eq!(applies("tui.theme"), "live");
    }
    #[test]
    fn assignment_edits_preserve_unknown_members() {
        let mut settings = Settings::default();
        settings.set_dotted(
            "agents.build",
            json!({"model":"old","variant":"off","future_option":42}),
        );
        apply(&mut settings, "agents.build", json!({"model":"new"}));
        assert_eq!(
            settings.get_dotted("agents.build"),
            Some(&json!({"model":"new","future_option":42}))
        );
        apply(&mut settings, "agents.build", Value::Null);
        assert!(settings.get_dotted("agents.build").is_none());
    }
    #[test]
    fn stored_variant_wire_round_trip() {
        let catalog: ModelsConfig = serde_json::from_value(json!({"providers":{"p":{"base_url":"http://localhost"}},"models":{"p/m":{"provider":"p","model":"m","variants":{"high":{}}}}})).unwrap();
        let agents = Agents::load(
            &crate::agent::definitions::AgentRoots {
                home: None,
                config_dir: None,
            },
            Path::new("/tmp/opencode"),
        );
        for (raw, expected) in [
            (json!({"model":"p/m"}), Value::Null),
            (json!({"model":"p/m","variant":null}), json!("off")),
            (json!({"model":"p/m","variant":"off"}), json!("off")),
            (json!({"model":"p/m","variant":"high"}), json!("high")),
            (json!("p/m"), Value::Null),
        ] {
            let wire = choice(&raw);
            assert_eq!(wire["variant"], expected);
            let stored = validate("layers.metacognition", &wire, &catalog, &agents, &[]).unwrap();
            assert_eq!(choice(&stored), wire);
        }
    }
}
