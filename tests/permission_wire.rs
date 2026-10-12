use serde_json::json;
use withnav::agent::permissions::PermissionDecision;
use withnav::bus::event::{PermissionRequested, PermissionResolution};

#[test]
fn new_and_legacy_answers_round_trip() {
    for name in [
        "allow_once",
        "allow_always",
        "allow_folder",
        "allow_config_session",
        "allow_skill_machine",
        "deny",
        "reject",
    ] {
        let mut decision = json!({"decision": name});
        if name == "deny" || name == "reject" {
            decision["message"] = json!("Try another approach");
        }
        let typed: PermissionDecision = serde_json::from_value(decision.clone()).unwrap();
        assert_eq!(serde_json::to_value(&typed).unwrap(), decision);
        let resolution = PermissionResolution::from(&typed);
        let wire = serde_json::to_value(&resolution).unwrap();
        assert_eq!(wire["resolution"], name);
        assert_eq!(
            serde_json::from_value::<PermissionResolution>(wire).unwrap(),
            resolution
        );
    }
    assert_eq!(
        serde_json::from_value::<PermissionDecision>(json!({"decision":"reject"})).unwrap(),
        PermissionDecision::Reject { message: None }
    );
    assert_eq!(
        serde_json::from_value::<PermissionResolution>(json!({"resolution":"reject"})).unwrap(),
        PermissionResolution::Reject { message: None }
    );
}

#[test]
fn historical_requests_need_no_always_label() {
    let mut wire = json!({"request_id":ulid::Ulid(1), "tool_call_id":"call", "tool":"bash", "arguments":"{}", "scope":{"kind":"folder","value":"/workspace"}, "agent":"build", "reason":"review"});
    assert!(serde_json::from_value::<PermissionRequested>(wire.clone())
        .unwrap()
        .always_label
        .is_none());
    wire["always_label"] = json!("Always (project): bash git push *");
    let request: PermissionRequested = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), wire);
}
