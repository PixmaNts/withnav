use instinct_flow::definition::{FallbackAction, NodeKind};

#[test]
fn old_human_action_is_normalized_in_nodes_and_fallbacks() {
    let node: NodeKind = serde_json::from_value(serde_json::json!({
        "type":"action", "action":"ask_driver", "params":{}
    }))
    .unwrap();
    assert!(matches!(node, NodeKind::Action { action, .. } if action == "ask_captain"));
    let fallback: FallbackAction = serde_json::from_value(serde_json::json!({
        "action":"ask_driver", "params":{}
    }))
    .unwrap();
    assert_eq!(fallback.action, "ask_captain");
}
