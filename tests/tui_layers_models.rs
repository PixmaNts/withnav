use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use navai::tui::{
    catalog::{self, Row},
    commands::{self, Routed},
    input::{route_key, Editor},
    protocol::*,
    reducer,
    state::{State, UiState, VariantPick},
};
use serde_json::json;

fn models() -> ModelsBody {
    serde_json::from_value(json!({"default_agent":"plan","models":[
        {"name":"omniroute/combo/astra","gateway":"omniroute","api":"chat","provider_group":"combo","upstream_id":"combo/astra","context_window":1000000,"variants":["low"]},
        {"name":"openai/astra","gateway":"openai","api":"chat","provider_group":"openai","upstream_id":"astra","context_window":1000000}
    ]})).unwrap()
}

fn mixed_models() -> ModelsBody {
    serde_json::from_value(json!({"default_agent":"plan","models":[
        {"name":"gateway/chat","gateway":"gateway","provider_group":"vendor","upstream_id":"chat","api":"chat","enabled":true,"context_window":100000},
        {"name":"gateway/fim","gateway":"gateway","provider_group":"vendor","upstream_id":"fim","api":"fim","enabled":true,"context_window":100000},
        {"name":"gateway/systemone","gateway":"gateway","provider_group":"vendor","upstream_id":"systemone","api":"systemone","enabled":true,"context_window":100000}
    ]})).unwrap()
}

fn live_session() -> SessionMetadata {
    serde_json::from_value(json!({
        "session_id":"S", "actor_state":"idle", "workspace":"", "model":"gateway/chat", "current_seq":0
    })).unwrap()
}

#[test]
fn mixed_catalog_retains_layer_models_but_conversation_rows_only_offer_chat() {
    let mut s = State::default();
    let generation = s.generation.clone();
    reducer::update(
        &mut s,
        ClientEvent::Models {
            workspace: String::new(),
            request: 0,
            generation,
            result: Ok(mixed_models()),
        },
    )
    .unwrap();
    assert_eq!(
        s.catalog.len(),
        3,
        "layer capabilities remain in the full catalog"
    );
    assert_eq!(
        s.catalog.iter().map(|m| m.api).collect::<Vec<_>>(),
        vec![ApiKind::Chat, ApiKind::Fim, ApiKind::SystemOne]
    );
    let mut ui = UiState::default();
    let keys = |ui: &UiState| {
        catalog::rows(&s.catalog, ui)
            .iter()
            .map(Row::key)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(&ui),
        vec!["group:gateway", "group:gateway/vendor", "gateway/chat"]
    );
    ui.collapsed_models.insert("gateway/vendor".into());
    assert_eq!(keys(&ui), vec!["group:gateway", "group:gateway/vendor"]);
    ui.filter = "CHAT".into();
    assert_eq!(
        keys(&ui),
        vec!["group:gateway", "group:gateway/vendor", "gateway/chat"]
    );
    for filter in ["fim", "systemone"] {
        ui.filter = filter.into();
        assert!(keys(&ui).is_empty());
    }
}

#[test]
fn missing_and_future_api_are_not_conversation_choices() {
    let mut body = serde_json::to_value(mixed_models()).unwrap();
    body["models"][0].as_object_mut().unwrap().remove("api");
    body["models"][1]["api"] = json!("future-api");
    let models: ModelsBody = serde_json::from_value(body).unwrap();
    assert_eq!(models.models[0].api, ApiKind::Unknown);
    assert_eq!(models.models[1].api, ApiKind::Unknown);
    assert!(catalog::rows(&models.models, &UiState::default()).is_empty());
}

#[test]
fn enter_and_ctrl_n_cannot_route_lower_layer_models() {
    for occupied in [false, true] {
        for (code, modifiers) in [
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Char('n'), KeyModifiers::CONTROL),
        ] {
            for filter in ["fim", "systemone"] {
                let mut s = State {
                    catalog: mixed_models().models,
                    ..State::default()
                };
                if occupied {
                    s.session = Some(live_session());
                }
                let mut ui = UiState {
                    overlay: Some(Overlay::Models),
                    filter: filter.into(),
                    ..UiState::default()
                };
                let action = route_key(
                    KeyEvent::new(code, modifiers),
                    &s,
                    &mut ui,
                    &mut Editor::default(),
                );
                assert_eq!(action, None);
                assert_eq!(ui.variant_pick, None);
                assert_eq!(ui.overlay, Some(Overlay::Models));
                assert!(s.start_model.is_none());
                assert!(s.model_switch.is_none());
                assert_eq!(s.tabs.len(), 1);
            }
        }
    }
}

#[test]
fn mixed_catalog_picker_renders_and_routes_chat_only() {
    use ratatui::{backend::TestBackend, Terminal};
    let mut s = State {
        catalog: mixed_models().models,
        ..State::default()
    };
    s.session = Some(live_session());
    let mut ui = UiState {
        overlay: Some(Overlay::Models),
        ..UiState::default()
    };
    ui.selection.insert(format!("{:?}", Overlay::Models), 2);
    let mut editor = Editor::default();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| {
            navai::tui::view::draw(
                frame,
                &s,
                &ui,
                &editor,
                &mut navai::tui::view::RenderCache::default(),
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..40)
        .map(|y| {
            (0..120)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("gateway/chat"));
    assert!(!text.contains("gateway/fim"));
    assert!(!text.contains("gateway/systemone"));
    assert_eq!(
        route_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::SwitchModel("gateway/chat".into()))
    );
    ui.overlay = Some(Overlay::Models);
    assert_eq!(
        route_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &s,
            &mut ui,
            &mut editor
        ),
        None
    );
    assert_eq!(
        ui.variant_pick,
        Some(VariantPick::NewConversation("gateway/chat".into()))
    );
    assert_eq!(
        route_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        ),
        Some(Action::PickModel("gateway/chat".into()))
    );
}

#[test]
fn direct_lower_layer_model_actions_are_rejected_before_tab_or_switch_effects() {
    for occupied in [false, true] {
        for name in ["gateway/fim", "gateway/systemone", "gateway/unknown"] {
            for action in [
                Action::PickModel(name.into()),
                Action::SwitchModel(name.into()),
            ] {
                let mut s = State {
                    catalog: mixed_models().models,
                    ..State::default()
                };
                s.catalog.push(ModelEntry {
                    name: "gateway/unknown".into(),
                    ..ModelEntry::default()
                });
                if occupied {
                    s.session = Some(live_session());
                }
                s.selected_model_id = Some("gateway/chat".into());
                let tabs = s.tabs.len();
                let generation = s.generation.clone();
                assert!(reducer::handle_action(&mut s, &mut UiState::default(), action).is_empty());
                assert_eq!(s.tabs.len(), tabs);
                assert_eq!(s.generation, generation);
                assert_eq!(s.selected_model_id.as_deref(), Some("gateway/chat"));
                assert!(s.start_model.is_none());
                assert!(s.model_switch.is_none());
                assert!(matches!(&s.conversation.blocks.last().unwrap().kind,
                    navai::tui::state::BlockKind::Notice(text) if text.contains(name) && text.contains("chat API")));
            }
        }
    }
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
fn alt_a_folds_every_group_then_unfolds_them_all() {
    let state = State {
        catalog: models().models,
        ..State::default()
    };
    let mut ui = UiState {
        overlay: Some(Overlay::Models),
        ..UiState::default()
    };
    let mut editor = Editor::default();
    let keys = |ui: &UiState| {
        catalog::rows(&state.catalog, ui)
            .iter()
            .map(Row::key)
            .collect::<Vec<_>>()
    };
    let all = keys(&ui);
    assert_eq!(all.len(), 5);
    // Select the model under the omniroute/combo provider (row 2).
    ui.selection.insert("Models".into(), 2);
    let alt_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
    route_key(alt_a, &state, &mut ui, &mut editor);
    assert_eq!(keys(&ui), vec!["group:omniroute", "group:openai"]);
    assert_eq!(ui.selection["Models"], 0, "lands on the gateway header");
    assert!(ui.filter.is_empty());
    route_key(alt_a, &state, &mut ui, &mut editor);
    assert_eq!(keys(&ui), all);
    assert!(ui.collapsed_models.is_empty());
}

#[test]
fn configured_default_agent_creates_without_editor_or_model_inheritance() {
    let mut s = State::default();
    let g = s.generation.clone();
    reducer::update(
        &mut s,
        ClientEvent::Models {
            workspace: String::new(),
            request: 0,
            generation: g.clone(),
            result: Ok(models()),
        },
    )
    .unwrap();
    reducer::update(
        &mut s,
        ClientEvent::Agents {
            workspace: String::new(),
            request: 0,
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
    assert!(body.get("agent").is_none());
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
            workspace: String::new(),
            request: 0,
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
            workspace: String::new(),
            request: 0,
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

fn hierarchy_models() -> ModelsBody {
    serde_json::from_value(json!({"default_agent":"plan","models":[
        {"name":"omniroute/cc/claude-opus","gateway":"omniroute","api":"chat","provider_group":"cc","upstream_id":"cc/claude-opus","context_window":200000},
        {"name":"omniroute/kimi/k2","gateway":"omniroute","api":"chat","provider_group":"kimi","upstream_id":"kimi/k2","context_window":200000},
        {"name":"omniroute/kimi/fim","gateway":"omniroute","api":"fim","provider_group":"kimi","upstream_id":"kimi/fim","context_window":200000},
        {"name":"direct/solo","gateway":"direct","api":"chat","provider_group":"direct","upstream_id":"solo","context_window":200000}
    ]})).unwrap()
}

/// The screen's rows as drawn; the selection mark is blanked.
fn picker_lines(state: &State, ui: &UiState, size: (u16, u16)) -> Vec<String> {
    use ratatui::{backend::TestBackend, Terminal};
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    terminal
        .draw(|frame| {
            navai::tui::view::draw(
                frame,
                state,
                ui,
                &Editor::default(),
                &mut navai::tui::view::RenderCache::default(),
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..size.1)
        .map(|y| {
            (0..size.0)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .map(|line| line.replace('▌', " "))
        .collect()
}

/// Column of `needle` inside the picker row that contains it.
fn column(lines: &[String], needle: &str) -> usize {
    let line = lines
        .iter()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle} not drawn in {lines:#?}"));
    line[..line.find(needle).unwrap()].chars().count()
}

#[test]
fn picker_indents_gateway_provider_and_model_and_direct_models_one_level() {
    let state = State {
        catalog: hierarchy_models().models,
        ..State::default()
    };
    let ui = UiState {
        overlay: Some(Overlay::Models),
        ..UiState::default()
    };
    let lines = picker_lines(&state, &ui, (100, 30));
    let gateway = column(&lines, "▾ omniroute");
    assert_eq!(column(&lines, "▾ cc"), gateway + 2);
    assert_eq!(column(&lines, "▾ kimi"), gateway + 2);
    assert_eq!(column(&lines, "omniroute/cc/cla"), gateway + 4);
    assert_eq!(column(&lines, "omniroute/kimi/k2"), gateway + 4);
    assert_eq!(column(&lines, "▾ direct"), gateway);
    assert_eq!(column(&lines, "direct/solo"), gateway + 2);
    assert_eq!(
        lines.iter().filter(|l| l.contains("▾ direct")).count(),
        1,
        "no redundant provider row for a direct gateway"
    );
    assert!(!lines.iter().any(|l| l.contains("kimi/fim")), "chat only");
    insta::assert_snapshot!(
        "model-picker-hierarchy",
        picker_lines(&state, &ui, (60, 16)).join("\n")
    );
    let levels: Vec<u8> = catalog::rows(&state.catalog, &ui)
        .iter()
        .map(Row::level)
        .collect();
    assert_eq!(levels, vec![0, 1, 0, 1, 2, 1, 2]);
}

#[test]
fn search_keeps_levels_and_collapsed_gateway_hides_its_provider_headers() {
    let state = State {
        catalog: hierarchy_models().models,
        ..State::default()
    };
    let mut ui = UiState {
        overlay: Some(Overlay::Models),
        ..UiState::default()
    };
    ui.collapsed_models.insert("omniroute/kimi".into());
    let lines = picker_lines(&state, &ui, (100, 30));
    let gateway = column(&lines, "▾ omniroute");
    assert_eq!(
        column(&lines, "▸ kimi"),
        gateway + 2,
        "collapsed provider stays indented"
    );
    assert!(!lines.iter().any(|l| l.contains("omniroute/kimi/k2")));

    ui.collapsed_models.insert("omniroute".into());
    let lines = picker_lines(&state, &ui, (100, 30));
    assert!(lines.iter().any(|l| l.contains("▸ omniroute")));
    assert!(!lines
        .iter()
        .any(|l| l.contains(" cc") || l.contains("kimi")));

    // A search opens every group and keeps each match at its level.
    ui.filter = "k2".into();
    let lines = picker_lines(&state, &ui, (100, 30));
    let gateway = column(&lines, "▾ omniroute");
    assert_eq!(column(&lines, "▾ kimi"), gateway + 2);
    assert_eq!(column(&lines, "omniroute/kimi/k2"), gateway + 4);
    assert!(!lines.iter().any(|l| l.contains("▾ cc")));
}

#[test]
fn narrow_terminal_with_long_canonical_names_keeps_indentation_without_panic() {
    let mut catalog = hierarchy_models().models;
    catalog[0].name = format!("omniroute/cc/{}", "very-long-model-name-".repeat(6));
    catalog[0].upstream_id = format!("cc/{}", "very-long-model-name-".repeat(6));
    let state = State {
        catalog,
        ..State::default()
    };
    let ui = UiState {
        overlay: Some(Overlay::Models),
        ..UiState::default()
    };
    // Below 24 columns the view asks for a larger terminal; from there the popup draws.
    for width in [8, 23, 24, 25, 30, 40, 60] {
        let lines = picker_lines(&state, &ui, (width, 20));
        if width >= 24 {
            let gateway = column(&lines, "▾ omni");
            assert_eq!(column(&lines, "▾ cc"), gateway + 2, "width {width}");
            // The long model is the row under its provider: cut with `…`, still at level 2.
            let cc = lines.iter().position(|l| l.contains("▾ cc")).unwrap();
            let model = &lines[cc + 1];
            let start = model.chars().position(|c| c == 'o').unwrap();
            assert_eq!(start, gateway + 4, "width {width}: {model}");
            assert!(model.contains('…'), "width {width}: {model}");
        }
    }
}
