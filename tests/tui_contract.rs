use navai::tui::{
    protocol::*,
    state::{Activity, State},
};

#[test]
fn http_dtos_roundtrip_server_shapes() {
    let models = serde_json::json!({"models":[{"alias":"fast","model":"vendor/model","context_window":128000}],"default":"fast"});
    let dto: ModelsBody = serde_json::from_value(models.clone()).unwrap();
    assert_eq!(serde_json::to_value(dto).unwrap(), models);
    // An older server's payload (no catalog fields) parses and is written back as it was.
    // A newer one, with every catalog field, round-trips too.
    let full = serde_json::json!({"models":[{
        "alias":"fast","model":"vendor/model","context_window":1000000,"enabled":true,
        "window_source":"discovered.context_length","stale":true,"stale_age_secs":259200,
        "refresh_error":"provider API error (HTTP 503)","reported_output":128000,
        "max_input_tokens":900000,"discrepancy":{"configured":200000,"discovered":1000000}
    },{"alias":"off","model":"m","context_window":null,"enabled":false,"disabled_reason":"no window"}],"default":null});
    let dto: ModelsBody = serde_json::from_value(full.clone()).unwrap();
    assert_eq!(serde_json::to_value(dto).unwrap(), full);
    // Fields this build does not know yet are ignored, not an error.
    let future = serde_json::json!({"models":[{"alias":"a","model":"m","context_window":1,"next_field":{"x":1}}],"default":"a"});
    assert!(serde_json::from_value::<ModelsBody>(future).is_ok());
    let sessions = serde_json::json!({"sessions":[{"id":"01ABC","title":null,"created_at":"now","updated_at":"now","message_count":0,"live":false}]});
    let dto: SessionListBody = serde_json::from_value(sessions.clone()).unwrap();
    assert_eq!(serde_json::to_value(dto).unwrap(), sessions);
    let request = CreateSessionRequest {
        workspace: "/tmp".into(),
        model: Some("fast".into()),
        resume: None,
        agent: None,
        variant: None,
    };
    let body = serde_json::json!({"workspace":"/tmp","model":"fast"});
    assert_eq!(serde_json::to_value(&request).unwrap(), body);
    let response =
        serde_json::json!({"session_id":"01ABC","root_actor_id":"agent:main","actor_state":"idle"});
    assert_eq!(
        serde_json::from_value::<CreateSessionResponse>(response.clone()).unwrap(),
        serde_json::from_value::<CreateSessionResponse>(response).unwrap()
    );
}

#[test]
fn default_state_is_idle_without_cursor() {
    let state = State::default();
    assert_eq!(state.activity, Activity::Idle);
    assert_eq!(state.last_applied_seq, None);
}
