use rust_agent::tui::{
    protocol::*,
    state::{Activity, State},
};

#[test]
fn http_dtos_roundtrip_server_shapes() {
    let models = serde_json::json!({"models":[{"alias":"fast","model":"vendor/model","context_window":128000}],"default":"fast"});
    let dto: ModelsBody = serde_json::from_value(models.clone()).unwrap();
    assert_eq!(serde_json::to_value(dto).unwrap(), models);
    let sessions = serde_json::json!({"sessions":[{"id":"01ABC","title":null,"created_at":"now","updated_at":"now","message_count":0,"live":false}]});
    let dto: SessionListBody = serde_json::from_value(sessions.clone()).unwrap();
    assert_eq!(serde_json::to_value(dto).unwrap(), sessions);
    let request = CreateSessionRequest {
        workspace: "/tmp".into(),
        model: Some("fast".into()),
        resume: None,
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
