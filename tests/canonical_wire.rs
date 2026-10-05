use navai::bus::event::{ContextSnapshot, ModelChanged};
use serde_json::json;

#[test]
fn model_changed_round_trip_preserves_selection_provenance() {
    let identity = json!({
        "agent": "build", "model": "omniroute/combo/gpt-6-astra",
        "gateway": "omniroute", "upstream_id": "combo/gpt-6-astra",
        "variant": "high", "source": "override", "variant_source": "override"
    });
    let wire = json!({
        "operation_id": ulid::Ulid::new().to_string(), "revision": 7,
        "reason": "agent", "previous": identity, "new": identity,
        "variant": "high", "window": 1050000, "input_budget": 989308,
        "from_message": 3
    });
    let typed: ModelChanged = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), wire);
    assert!(!wire.to_string().contains("alias"));
    assert!(!wire.to_string().contains("\"provider\""));
}

#[test]
fn context_keeps_current_and_historical_usage_identity_separate() {
    let step = ulid::Ulid::new().to_string();
    let wire = json!({
        "model": "omniroute/combo/current", "gateway": "omniroute",
        "upstream_id": "combo/current", "window": 100000,
        "window_source": "models.json", "estimated": true, "estimate_total": 100,
        "breakdown": {"system":100,"rules":0,"skills_catalog":0,"tools":0,"messages":0},
        "output_reserve": 8192, "margin": 5000, "input_budget": 86808, "percent": 0,
        "last_usage": {
            "model": "direct/previous", "gateway": "direct", "upstream_id": "previous",
            "prompt_tokens": 90, "completion_tokens": 10, "step": step
        },
        "skills": {"listed":0,"omitted":0}, "rule_files":0,
        "incarnation": ulid::Ulid::new().to_string(), "revision":9
    });
    let typed: ContextSnapshot = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), wire);
    assert!(!wire.to_string().contains("alias"));
}
