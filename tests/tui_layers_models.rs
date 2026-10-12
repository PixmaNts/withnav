use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use withnav::tui::{
    catalog::{self, Row},
    commands::{self, Routed},
    input::{route_key, Editor},
    protocol::*,
    reducer,
    state::{State, UiState, VariantPick},
};

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

fn picker_state(models: ModelsBody) -> (State, UiState) {
    (
        State {
            catalog: models.models,
            ..State::default()
        },
        UiState {
            overlay: Some(Overlay::Models),
            ..UiState::default()
        },
    )
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
                let (mut s, mut ui) = picker_state(mixed_models());
                if occupied {
                    s.session = Some(live_session());
                }
                ui.filter = filter.into();
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
    let (mut s, mut ui) = picker_state(mixed_models());
    s.session = Some(live_session());
    ui.selection.insert(format!("{:?}", Overlay::Models), 2);
    let mut editor = Editor::default();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| {
            withnav::tui::view::draw(
                frame,
                &s,
                &ui,
                &editor,
                &mut withnav::tui::view::RenderCache::default(),
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
                    withnav::tui::state::BlockKind::Notice(text) if text.contains(name) && text.contains("chat API")));
            }
        }
    }
}
fn agents() -> AgentsBody {
    serde_json::from_value(json!({"default_agent":"plan","agents":[
        {"id":"plan","description":"Plan","mode":"primary","assignment":{"model":"openai/astra","variant":null,"source":"config","variant_source":"model_default"},"effective":{"model":"openai/astra","variant":null,"source":"config","variant_source":"model_default"},"warning":null},
        {"id":"vigie","description":"Explore","mode":"subagent","assignment":null,"effective":null,"warning":null}
    ]})).unwrap()
}

#[test]
fn gateway_groups_collapse_search_reopens_and_headers_never_pick_models() {
    let (state, mut ui) = picker_state(models());
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
    let (state, mut ui) = picker_state(models());
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
    let mut e: withnav::bus::event::EventEnvelope=serde_json::from_value(json!({
        "schema_version":1,"session_id":ulid::Ulid::generate(),"seq":1,"event_id":ulid::Ulid::generate(),"occurred_at":"2026-10-05T00:00:00Z", "kind":"model.changed",
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
    let client = withnav::tui::client::Client::new(format!("http://{address}"), None);
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
    let mut cache = withnav::tui::view::RenderCache::default();
    terminal
        .draw(|frame| {
            withnav::tui::view::draw(frame, &s, &UiState::default(), &Editor::new(), &mut cache)
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
        "Completion · FIM",
        "Triage",
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
            withnav::tui::view::draw(
                frame,
                state,
                ui,
                &Editor::default(),
                &mut withnav::tui::view::RenderCache::default(),
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
    let (state, ui) = picker_state(hierarchy_models());
    let lines = picker_lines(&state, &ui, (100, 30));
    let gateway = column(&lines, "▾ omniroute");
    assert_eq!(column(&lines, "▾ cc"), gateway + 2);
    assert_eq!(column(&lines, "▾ kimi"), gateway + 2);
    assert_eq!(column(&lines, "claude-opus"), gateway + 4);
    assert_eq!(column(&lines, "k2 "), gateway + 4);
    assert_eq!(column(&lines, "▾ direct"), gateway);
    assert_eq!(column(&lines, "solo"), gateway + 2);
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("omniroute/cc") || l.contains("direct/solo")),
        "a row in a group shows the short name only"
    );
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
    let (state, mut ui) = picker_state(hierarchy_models());
    ui.collapsed_models.insert("omniroute/kimi".into());
    let lines = picker_lines(&state, &ui, (100, 30));
    let gateway = column(&lines, "▾ omniroute");
    assert_eq!(
        column(&lines, "▸ kimi"),
        gateway + 2,
        "collapsed provider stays indented"
    );
    assert!(!lines.iter().any(|l| l.contains("k2")));

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
    let kimi = lines.iter().position(|l| l.contains("▾ kimi")).unwrap();
    assert_eq!(column(&lines[kimi + 1..], "k2"), gateway + 4);
    assert!(!lines.iter().any(|l| l.contains("▾ cc")));
    // Search still matches the full canonical name.
    ui.filter = "omniroute/kimi".into();
    let lines = picker_lines(&state, &ui, (100, 30));
    assert!(lines.iter().any(|l| l.contains("▾ kimi")));
    assert!(!lines.iter().any(|l| l.contains("▾ cc")));
}

#[test]
fn narrow_terminal_with_long_canonical_names_keeps_indentation_without_panic() {
    let (mut state, ui) = picker_state(hierarchy_models());
    state.catalog[0].name = format!("omniroute/cc/{}", "very-long-model-name-".repeat(6));
    state.catalog[0].upstream_id = format!("cc/{}", "very-long-model-name-".repeat(6));
    // Below 24 columns the view asks for a larger terminal; from there the popup draws.
    for width in [8, 23, 24, 25, 30, 40, 60] {
        let lines = picker_lines(&state, &ui, (width, 20));
        if width >= 24 {
            let gateway = column(&lines, "▾ omni");
            assert_eq!(column(&lines, "▾ cc"), gateway + 2, "width {width}");
            // The long model is the row under its provider: cut with `…`, still at level 2.
            let cc = lines.iter().position(|l| l.contains("▾ cc")).unwrap();
            let model = &lines[cc + 1];
            let start = model.chars().position(|c| c == 'v').unwrap();
            assert_eq!(start, gateway + 4, "width {width}: {model}");
            assert!(model.contains('…'), "width {width}: {model}");
        }
    }
}

fn ctrl_f() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
}

fn favorites_picker() -> (State, UiState) {
    picker_state(hierarchy_models())
}

fn keys_of(state: &State, ui: &UiState) -> Vec<String> {
    catalog::rows(&state.catalog, ui)
        .iter()
        .map(Row::key)
        .collect()
}

/// Opens the picker's row `index` and presses ctrl+f on it.
fn toggle_at(state: &State, ui: &mut UiState, index: usize) -> Option<Action> {
    ui.selection.insert("Models".into(), index);
    route_key(ctrl_f(), state, ui, &mut Editor::default())
}

#[test]
fn ctrl_f_adds_and_removes_a_favorite_and_the_section_comes_first() {
    let (state, mut ui) = favorites_picker();
    let plain = keys_of(&state, &ui);
    assert_eq!(plain[0], "group:direct", "no favorites, no section");
    // Row 6 is omniroute/kimi/k2.
    assert_eq!(plain[6], "omniroute/kimi/k2");
    let action = toggle_at(&state, &mut ui, 6);
    assert_eq!(
        action,
        Some(Action::SaveFavoriteModels(vec!["omniroute/kimi/k2".into()]))
    );
    let keys = keys_of(&state, &ui);
    assert_eq!(
        &keys[..2],
        ["section:favorites", "omniroute/kimi/k2"],
        "section first"
    );
    assert_eq!(keys.len(), plain.len() + 2);
    assert_eq!(
        keys.iter().filter(|k| *k == "omniroute/kimi/k2").count(),
        2,
        "also kept in its group"
    );
    // The highlight stays on the same model, in its group.
    assert_eq!(ui.selection["Models"], 8);
    assert_eq!(keys[8], "omniroute/kimi/k2");

    // From the section: the model leaves it, and the highlight lands on its group row.
    let action = toggle_at(&state, &mut ui, 1);
    assert_eq!(action, Some(Action::SaveFavoriteModels(vec![])));
    assert_eq!(
        keys_of(&state, &ui),
        plain,
        "last favorite hides the section"
    );
    assert_eq!(
        keys_of(&state, &ui)[ui.selection["Models"]],
        "omniroute/kimi/k2"
    );
}

#[test]
fn ctrl_f_on_a_heading_does_nothing() {
    let (state, mut ui) = favorites_picker();
    assert_eq!(toggle_at(&state, &mut ui, 0), None);
    assert!(ui.favorite_models.is_empty());
    ui.favorite_models.insert("direct/solo".into());
    assert_eq!(toggle_at(&state, &mut ui, 0), None, "section heading");
    assert_eq!(ui.favorite_models.len(), 1);
}

#[test]
fn favorites_are_sorted_chat_only_and_unknown_ones_stay_hidden() {
    let (state, mut ui) = favorites_picker();
    for name in [
        "omniroute/kimi/k2",
        "direct/solo",
        "omniroute/kimi/fim",
        "gone/model",
    ] {
        ui.favorite_models.insert(name.into());
    }
    let keys = keys_of(&state, &ui);
    assert_eq!(
        &keys[..3],
        ["section:favorites", "direct/solo", "omniroute/kimi/k2"]
    );
    assert!(!keys
        .iter()
        .any(|k| k == "omniroute/kimi/fim" || k == "gone/model"));
    assert_eq!(ui.favorite_models.len(), 4, "kept in storage");
}

#[test]
fn favorites_section_ignores_folding_and_alt_a() {
    let (state, mut ui) = favorites_picker();
    ui.favorite_models.insert("omniroute/cc/claude-opus".into());
    let mut editor = Editor::default();
    ui.selection.insert("Models".into(), 0);
    for code in [KeyCode::Enter, KeyCode::Left, KeyCode::Right] {
        assert_eq!(
            route_key(
                KeyEvent::new(code, KeyModifiers::NONE),
                &state,
                &mut ui,
                &mut editor
            ),
            None
        );
        assert!(ui.collapsed_models.is_empty(), "{code:?}");
        assert_eq!(ui.overlay, Some(Overlay::Models));
    }
    let alt_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
    route_key(alt_a, &state, &mut ui, &mut editor);
    let keys = keys_of(&state, &ui);
    assert_eq!(
        keys,
        [
            "section:favorites",
            "omniroute/cc/claude-opus",
            "group:direct",
            "group:omniroute"
        ]
    );
    assert_eq!(ui.selection["Models"], 0);
    // Selected on a favorite row, alt+a keeps the highlight there.
    ui.selection.insert("Models".into(), 1);
    route_key(alt_a, &state, &mut ui, &mut editor);
    assert!(ui.collapsed_models.is_empty());
    assert_eq!(
        keys_of(&state, &ui)[ui.selection["Models"]],
        "omniroute/cc/claude-opus"
    );
    assert!(ui.selection["Models"] >= 1);
}

#[test]
fn search_filters_the_favorites_section_and_hides_it_without_a_match() {
    let (state, mut ui) = favorites_picker();
    ui.favorite_models.insert("omniroute/cc/claude-opus".into());
    ui.favorite_models.insert("direct/solo".into());
    ui.filter = "solo".into();
    let keys = keys_of(&state, &ui);
    assert_eq!(
        keys,
        [
            "section:favorites",
            "direct/solo",
            "group:direct",
            "direct/solo"
        ]
    );
    ui.filter = "k2".into();
    assert!(!keys_of(&state, &ui).contains(&"section:favorites".to_string()));
}

#[test]
fn enter_on_a_favorite_row_picks_that_model_like_its_group_row() {
    let (state, mut ui) = favorites_picker();
    ui.favorite_models.insert("direct/solo".into());
    ui.selection.insert("Models".into(), 1);
    let mut editor = Editor::default();
    route_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &state,
        &mut ui,
        &mut editor,
    );
    let from_favorite = (ui.overlay, ui.variant_pick.clone());
    // The same model through its group row.
    let (_, mut other) = favorites_picker();
    other.selection.insert("Models".into(), 1);
    route_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &state,
        &mut other,
        &mut editor,
    );
    assert_eq!(from_favorite, (other.overlay, other.variant_pick.clone()));
    assert_eq!(
        ui.variant_pick,
        Some(VariantPick::NewConversation("direct/solo".into()))
    );
    // ctrl+n works on it as on any model row.
    let (_, mut ui) = favorites_picker();
    ui.favorite_models.insert("direct/solo".into());
    ui.selection.insert("Models".into(), 1);
    route_key(
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        &state,
        &mut ui,
        &mut editor,
    );
    assert_eq!(
        ui.variant_pick,
        Some(VariantPick::NewConversation("direct/solo".into()))
    );
}

#[test]
fn picker_marks_favorites_with_a_star_section_and_group() {
    let (state, mut ui) = favorites_picker();
    ui.favorite_models.insert("omniroute/kimi/k2".into());
    ui.favorite_models.insert("direct/solo".into());
    let lines = picker_lines(&state, &ui, (60, 20));
    let section = column(&lines, "★ Favorites");
    // The section shows the whole name; the group row below its header only the short one.
    for (full, short) in [("direct/solo", "solo"), ("omniroute/kimi/k2", "k2")] {
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains(&format!("★ {full}")))
                .count(),
            1,
            "{full} in the section: {lines:#?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains(&format!("★ {short} ")))
                .count(),
            1,
            "{short} in its group: {lines:#?}"
        );
    }
    assert!(lines.iter().any(|l| l.contains("▾ direct")));
    assert!(
        section < column(&lines, "direct/solo"),
        "section models are indented"
    );
    let levels: Vec<u8> = catalog::rows(&state.catalog, &ui)
        .iter()
        .map(Row::level)
        .collect();
    assert_eq!(levels[..3], [0, 1, 1]);
    assert!(lines.iter().any(|l| l.contains("ctrl+f fav")));
    insta::assert_snapshot!("model-picker-favorites", lines.join("\n"));
}

fn long_name_models() -> ModelsBody {
    serde_json::from_value(json!({"default_agent":"plan","models":[
        {"name":"omniroute/agy/claude-opus-4-5-thinking","gateway":"omniroute","api":"chat","provider_group":"agy","upstream_id":"agy/claude-opus-4-5-thinking","context_window":1000000,"variants":["high","low","max","medium","none","xhigh"]},
        {"name":"omniroute/agy/gemini-3-pro","gateway":"omniroute","api":"chat","provider_group":"agy","upstream_id":"agy/gemini-3-pro","context_window":200000}
    ]})).unwrap()
}

/// The picker with the first model highlighted (row 0 is the gateway, 1 the provider, 2 the model).
fn long_name_picker(size: (u16, u16)) -> Vec<String> {
    let (state, mut ui) = picker_state(long_name_models());
    ui.selection.insert("Models".into(), 2);
    picker_lines(&state, &ui, size)
}

#[test]
fn long_model_name_shows_short_in_the_row_and_in_full_with_variants_in_the_detail_line() {
    let wide = long_name_picker((100, 16));
    let row = wide
        .iter()
        .find(|l| l.contains("claude-opus-4-5-thinking"))
        .unwrap();
    assert!(
        !row.contains("omniroute/agy/claude"),
        "short name in the row: {row}"
    );
    assert!(
        !row.contains('…') && !row.contains("high"),
        "no cut, no variants: {row}"
    );
    assert!(row.contains("1M"), "{row}");
    // The popup is at most 76 columns wide, so this long a line is cut at its end.
    let detail = wide.iter().find(|l| l.contains("· variants:")).unwrap();
    assert!(
        detail.contains("omniroute/agy/claude-opus-4-5-thinking · variants: high low max medium"),
        "{detail}"
    );
    assert!(detail.contains('…'), "{detail}");
    let keys = wide.iter().position(|l| l.contains("esc close")).unwrap();
    assert!(
        wide[keys - 1].contains("· variants:"),
        "detail sits just above the keys line"
    );
    insta::assert_snapshot!("model-picker-long-name-100", wide.join("\n"));

    let narrow = long_name_picker((60, 16));
    let detail = narrow
        .iter()
        .find(|l| l.contains("omniroute/agy/claude"))
        .unwrap();
    assert!(
        detail.contains('…'),
        "the detail cuts with an ellipsis: {detail}"
    );
    insta::assert_snapshot!("model-picker-long-name-60", narrow.join("\n"));
}

#[test]
fn detail_line_is_blank_on_a_header_and_has_no_variants_without_any() {
    let (state, mut ui) = picker_state(long_name_models());
    for row in [0, 1] {
        ui.selection.insert("Models".into(), row);
        let lines = picker_lines(&state, &ui, (100, 16));
        let keys = lines.iter().position(|l| l.contains("esc close")).unwrap();
        assert!(
            !lines[keys - 1].contains("omniroute/agy/"),
            "header row {row}: {}",
            lines[keys - 1]
        );
    }
    ui.selection.insert("Models".into(), 3);
    let lines = picker_lines(&state, &ui, (100, 16));
    let keys = lines.iter().position(|l| l.contains("esc close")).unwrap();
    assert!(lines[keys - 1].contains("omniroute/agy/gemini-3-pro"));
    assert!(!lines[keys - 1].contains("variants"));
}

#[test]
fn favorites_rows_show_the_full_name_and_the_detail_line_follows_them() {
    let (state, mut ui) = picker_state(long_name_models());
    ui.favorite_models
        .insert("omniroute/agy/gemini-3-pro".into());
    ui.selection.insert("Models".into(), 1);
    let lines = picker_lines(&state, &ui, (100, 16));
    assert!(lines
        .iter()
        .any(|l| l.contains("★ omniroute/agy/gemini-3-pro")));
    let keys = lines.iter().position(|l| l.contains("esc close")).unwrap();
    assert!(lines[keys - 1].contains("omniroute/agy/gemini-3-pro"));
}

#[test]
fn keys_line_never_cuts_a_word_and_keeps_esc_close() {
    for width in [60, 70, 100] {
        let lines = long_name_picker((width, 16));
        let keys = lines
            .iter()
            .find(|l| l.contains("↑↓ move"))
            .unwrap_or_else(|| panic!("no keys line at {width}: {lines:#?}"));
        let words = keys.split("│").nth(1).unwrap().trim();
        assert!(words.contains("ctrl+f fav"), "{width}: {words}");
        assert!(words.ends_with("esc close"), "{width}: {words}");
        for item in [
            "↑↓ move",
            "←→ fold",
            "alt+a all",
            "ctrl+f fav",
            "enter choose",
            "esc close",
        ] {
            let start = item.split(' ').next().unwrap();
            if words.contains(start) {
                assert!(words.contains(item), "{width}: cut item {item} in {words}");
            }
        }
    }
    // Below the full line's width, whole items drop, never a part of one.
    let lines = long_name_picker((40, 16));
    let keys = lines.iter().find(|l| l.contains("↑↓ move")).unwrap();
    assert!(keys.contains("esc close"), "{keys}");
    assert!(!keys.contains("enter choo "), "{keys}");
}

#[test]
fn detail_line_shows_the_whole_name_and_variants_when_they_fit() {
    let (mut state, mut ui) = picker_state(long_name_models());
    state.catalog[0].name = "omniroute/agy/claude-opus-4-5".into();
    ui.selection.insert("Models".into(), 2);
    let lines = picker_lines(&state, &ui, (100, 16));
    assert!(
        lines.iter().any(|l| l
            .contains("omniroute/agy/claude-opus-4-5 · variants: high low max medium none xhigh")),
        "{lines:#?}"
    );
}
