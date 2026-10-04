use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use navai::tui::{
    catalog::{self, Row},
    commands::{self, Routed},
    input::{route_key, Editor},
    protocol::*,
    reducer,
    state::{State, UiState},
};
use serde_json::json;

fn models() -> ModelsBody {
    serde_json::from_value(json!({"default_agent":"plan","models":[
        {"name":"omniroute/combo/astra","gateway":"omniroute","provider_group":"combo","upstream_id":"combo/astra","context_window":1000000,"variants":["low"]},
        {"name":"openai/astra","gateway":"openai","provider_group":"openai","upstream_id":"astra","context_window":1000000}
    ]})).unwrap()
}
fn agents() -> AgentsBody {
    serde_json::from_value(json!({"default_agent":"plan","agents":[
        {"id":"plan","description":"Plan","mode":"primary","assignment":{"model":"openai/astra","variant":null,"source":"config","variant_source":"model_default"},"effective":{"model":"openai/astra","variant":null,"source":"config","variant_source":"model_default"},"warning":null},
        {"id":"explore","description":"Explore","mode":"subagent","assignment":null,"effective":null,"warning":null}
    ]})).unwrap()
}

#[test]
fn gateway_groups_collapse_search_reopens_and_headers_never_pick_models() {
    let state = State {
        catalog: models().models,
        ..State::default()
    };
    let mut ui = UiState {
        overlay: Some(Overlay::Models),
        ..UiState::default()
    };
    let mut editor = Editor::default();
    assert_eq!(catalog::rows(&state.catalog, &ui).len(), 5);
    route_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &state,
        &mut ui,
        &mut editor,
    );
    assert!(ui.collapsed_models.contains("omniroute"));
    assert_eq!(catalog::rows(&state.catalog, &ui).len(), 3);
    ui.filter = "combo".into();
    assert!(catalog::rows(&state.catalog, &ui)
        .iter()
        .any(|r| matches!(r,Row::Model(m) if m.name=="omniroute/combo/astra")));
    assert!(state.start_model.is_none());
}

#[test]
fn configured_default_agent_creates_without_editor_or_model_inheritance() {
    let mut s = State::default();
    let g = s.generation.clone();
    reducer::update(
        &mut s,
        ClientEvent::Models {
            generation: g.clone(),
            result: Ok(models()),
        },
    )
    .unwrap();
    reducer::update(
        &mut s,
        ClientEvent::Agents {
            generation: g,
            session_id: None,
            result: Ok(agents()),
        },
    )
    .unwrap();
    assert_eq!(s.agent_name(), "plan");
    assert_eq!(s.selected_model_id.as_deref(), Some("openai/astra"));
    let effects = reducer::handle_action(
        &mut s,
        &mut UiState::default(),
        Action::Submit("hello".into()),
    );
    let request = effects
        .iter()
        .find_map(|e| {
            if let Effect::Create(r) = e {
                Some(r)
            } else {
                None
            }
        })
        .unwrap();
    let body = serde_json::to_value(request).unwrap();
    assert_eq!(body["agent"], "plan");
    assert!(body.get("model").is_none());
    assert!(body.get("editor").is_none());
    assert!(
        matches!(commands::route("/new plan",Mode::NextTurn),Routed::Local(Action::PickAgent(a)) if a=="plan")
    );
}

#[test]
fn stale_assignment_fetch_cannot_change_another_generation_or_session() {
    let mut s = State::default();
    reducer::update(
        &mut s,
        ClientEvent::Agents {
            generation: Generation(99),
            session_id: None,
            result: Ok(agents()),
        },
    )
    .unwrap();
    assert!(s.agents.is_empty());
    let g = s.generation.clone();
    reducer::update(
        &mut s,
        ClientEvent::Agents {
            generation: g,
            session_id: Some("foreign".into()),
            result: Ok(agents()),
        },
    )
    .unwrap();
    assert!(s.agents.is_empty());
}

#[test]
fn unified_selection_replay_restores_agent_model_variant_and_override_source() {
    let mut s = State::default();
    let mut e: navai::bus::event::EventEnvelope=serde_json::from_value(json!({
        "schema_version":1,"session_id":ulid::Ulid::new(),"seq":1,"event_id":ulid::Ulid::new(),"occurred_at":"2026-10-05T00:00:00Z", "kind":"model.changed",
        "actor_id":"agent:main","payload":{"previous":null,"new":{"agent":"plan","model":"omniroute/combo/astra","gateway":"omniroute","upstream_id":"combo/astra","variant":"low","source":"override","variant_source":"override"},"window":1000000,"reason":"agent","operation_id":null}
    })).unwrap();
    e.seq = 1;
    let g = s.generation.clone();
    reducer::update(
        &mut s,
        ClientEvent::Envelope {
            generation: g,
            envelope: e,
        },
    )
    .unwrap();
    assert_eq!(s.agent_name(), "plan");
    assert_eq!(
        s.selected_model_id.as_deref(),
        Some("omniroute/combo/astra")
    );
    assert_eq!(s.variant.as_deref(), Some("low"));
    assert_eq!(s.model_source, "override");
}

#[tokio::test]
async fn target_http_catalog_agents_and_layers_use_workspace_and_session_queries() {
    use axum::{extract::Query, routing::get, Json, Router};
    use std::collections::HashMap;
    let app=Router::new()
        .route("/v1/models",get(|| async { Json(models()) }))
        .route("/v1/agents",get(|Query(query):Query<HashMap<String,String>>| async move {
            assert_eq!(query.get("workspace").map(String::as_str),Some("/project"));
            assert_eq!(query.get("session_id").map(String::as_str),Some("S"));
            Json(agents())
        }))
        .route("/v1/layers",get(|| async { Json(json!({"layers":[
            {"layer":"reflex","status":"usable","model":"inception/mercury","variant":null,"reason":null},
            {"layer":"instinct","status":"disabled","model":"gateway/jev","variant":null,"reason":"wrong API kind"},
            {"layer":"metacognition","status":"usable","model":"omniroute/combo/astra","variant":"low","reason":null}
        ]})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = navai::tui::client::Client::new(format!("http://{address}"), None);
    assert_eq!(
        client.models("/project").await.unwrap().models[0].name,
        "omniroute/combo/astra"
    );
    assert_eq!(
        client
            .agents("/project", Some("S"))
            .await
            .unwrap()
            .default_agent,
        "plan"
    );
    let layers = client.layers("/project").await.unwrap();
    assert_eq!(layers.layers[1].reason.as_deref(), Some("wrong API kind"));
    task.abort();
}

#[test]
fn panel_shows_config_layer_capability_not_network_status_and_agent_override() {
    use ratatui::{backend::TestBackend, Terminal};
    let mut s = State::default();
    s.tabs[0].selected_model_id = Some("openai/astra".into());
    s.tabs[0].model_source = "override".into();
    s.layers=serde_json::from_value::<LayersBody>(json!({"layers":[
        {"layer":"reflex","status":"usable","model":"inception/mercury","variant":null,"reason":null},
        {"layer":"instinct","status":"disabled","model":"gateway/jev","variant":null,"reason":"wrong API kind"}
    ]})).unwrap().layers;
    let mut terminal = Terminal::new(TestBackend::new(160, 45)).unwrap();
    let mut cache = navai::tui::view::RenderCache::default();
    terminal
        .draw(|frame| {
            navai::tui::view::draw(frame, &s, &UiState::default(), &Editor::new(), &mut cache)
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..45)
        .map(|y| {
            (0..160)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    for value in [
        "override",
        "FIM · editor only",
        "System One",
        "usable",
        "disabled",
        "wrong API kind",
        "config.json",
    ] {
        assert!(text.contains(value), "{value}: {text}");
    }
    assert!(!text.contains("online"));
}
