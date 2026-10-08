use withnav::tui::{
    protocol::*,
    state::{Activity, State},
};

#[test]
fn http_dtos_roundtrip_server_shapes() {
    let models = serde_json::json!({"models":[{"name":"gateway/vendor/model","gateway":"gateway","api":"chat","provider_group":"vendor","upstream_id":"vendor/model","context_window":128000}],"default_agent":"build"});
    let dto: ModelsBody = serde_json::from_value(models.clone()).unwrap();
    assert_eq!(serde_json::to_value(dto).unwrap(), models);
    // An older server's payload (no catalog fields) parses and is written back as it was.
    // A newer one, with every catalog field, round-trips too.
    let full = serde_json::json!({"models":[{
        "name":"gateway/vendor/model","gateway":"gateway","api":"chat","provider_group":"vendor","upstream_id":"vendor/model","context_window":1000000,"enabled":true,
        "window_source":"discovered.context_length","stale":true,"stale_age_secs":259200,
        "refresh_error":"provider API error (HTTP 503)","reported_output":128000,
        "max_input_tokens":900000,"discrepancy":{"configured":200000,"discovered":1000000}
    },{"name":"gateway/m","gateway":"gateway","api":"chat","provider_group":"gateway","upstream_id":"m","context_window":null,"enabled":false,"disabled_reason":"no window"}],"default_agent":null});
    let dto: ModelsBody = serde_json::from_value(full.clone()).unwrap();
    assert_eq!(serde_json::to_value(dto).unwrap(), full);
    // Fields this build does not know yet are ignored, not an error.
    let future = serde_json::json!({"models":[{"name":"gateway/m","gateway":"gateway","api":"chat","provider_group":"gateway","upstream_id":"m","context_window":1,"next_field":{"x":1}}],"default_agent":"build"});
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

#[test]
fn the_panel_gives_the_primary_agents_the_roles_the_server_defines() {
    let server = withnav::agent::definitions::Agents::builtin();
    for (id, role) in withnav::tui::view::panel::PRIMARY_ROLES {
        let definition = server.get(id).unwrap_or_else(|| panic!("{id} is built in"));
        assert_eq!(definition.description, role, "{id}");
        // no agent file can take these names, so the role shown is the one in use
        assert!(withnav::agent::definitions::AgentId::parse(id).is_ok_and(|a| a.is_reserved()));
    }
}

#[test]
fn the_terminal_client_imports_no_agent_session_or_runtime_code() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui"),
        &mut files,
    );
    assert!(!files.is_empty());
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for banned in ["crate::agent", "crate::session", "crate::bus::runtime"] {
            assert!(!text.contains(banned), "{}: {banned}", file.display());
        }
    }
}
