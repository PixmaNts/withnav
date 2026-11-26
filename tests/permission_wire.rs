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
    assert!(
        serde_json::from_value::<PermissionRequested>(wire.clone())
            .unwrap()
            .always_label
            .is_none()
    );
    wire["always_label"] = json!("Always (project): bash git push *");
    let request: PermissionRequested = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), wire);
}

#[test]
fn an_always_resolution_without_the_saved_field_was_saved() {
    let old: PermissionResolution =
        serde_json::from_value(json!({"resolution":"allow_always"})).unwrap();
    assert_eq!(
        old,
        PermissionResolution::AllowAlways {
            always_saved: true,
            always_error: None
        }
    );
    let failed = PermissionResolution::AllowAlways {
        always_saved: false,
        always_error: Some("database is locked".into()),
    };
    assert_eq!(
        serde_json::to_value(&failed).unwrap(),
        json!({"resolution":"allow_always","always_saved":false,"always_error":"database is locked"})
    );
}

// The parallel review's event is `permission.review`: its state and, for a `wait`, three short
// lines. A request names the call digest and the turn an answer is bound to; a client built
// before these fields reads the request as before.
#[test]
fn the_review_event_and_the_bound_request_keep_their_wire_shape() {
    use withnav::bus::event::{PermissionReview, PermissionReviewState};
    let request_id = ulid::Ulid(7);
    let review = PermissionReview {
        request_id,
        tool_call_id: "call".into(),
        child_session_id: None,
        state: PermissionReviewState::Wait,
        conclusion: Some("The push goes beyond the request.".into()),
        basis: Some("tidy the notes".into()),
        consequence: Some("publishes commits".into()),
    };
    assert_eq!(
        serde_json::to_value(&review).unwrap(),
        json!({
            "request_id": request_id,
            "tool_call_id": "call",
            "state": "wait",
            "conclusion": "The push goes beyond the request.",
            "basis": "tidy the notes",
            "consequence": "publishes commits",
        })
    );
    for state in ["reviewing", "wait", "unavailable", "held"] {
        let wire = json!({"request_id": request_id, "tool_call_id": "c", "state": state});
        let parsed: PermissionReview = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), wire);
    }
    let bound = json!({"request_id":request_id, "tool_call_id":"call", "tool":"bash", "arguments":"{}", "scope":{"kind":"folder","value":"/workspace"}, "agent":"build", "reason":"review", "call_digest": "ab".repeat(32), "turn_id": ulid::Ulid(9)});
    let request: PermissionRequested = serde_json::from_value(bound).unwrap();
    assert_eq!(request.request_id, request_id);
}
