use serde_json::json;
use withnav_tui::{
    protocol::*,
    reducer,
    state::{Activity, State, UiState, VariantPick},
};

fn summary(seq: u64, agent: &str, model: &str, variant: Option<&str>) -> SessionMetadata {
    serde_json::from_value(json!({"session_id":"S","actor_state":"idle","workspace":"/ws","agent":agent,"model":model,"variant":variant,"source":"config","variant_source":"model_default","current_seq":seq})).unwrap()
}
fn event(s: &mut State, seq: u64, kind: &str, payload: serde_json::Value) {
    let envelope = withnav_client_api::event::EventEnvelope::new(
        ulid::Ulid::generate(),
        seq,
        withnav_client_api::event::EventKind::ModelChanged,
        payload,
    );
    let mut envelope = envelope;
    envelope.kind = kind.into();
    reducer::update(
        s,
        ClientEvent::Envelope {
            generation: s.generation.clone(),
            envelope,
        },
    )
    .unwrap();
}
fn metadata(s: &mut State, m: SessionMetadata) {
    reducer::update(
        s,
        ClientEvent::Metadata {
            generation: s.generation.clone(),
            result: Ok(m),
        },
    )
    .unwrap();
}
fn tuple(s: &State) -> (String, Option<String>, Option<String>, String, String) {
    (
        s.agent_name().into(),
        s.selected_model_id.clone(),
        s.variant.clone(),
        s.model_source.clone(),
        s.variant_source.clone(),
    )
}
fn change(agent: &str, model: &str) -> serde_json::Value {
    json!({"new":{"agent":agent,"model":model,"variant":"high","source":"override","variant_source":"override"},"previous":null,"window":1000000,"reason":"agent"})
}

#[test]
fn complete_selection_is_atomic_across_summary_replay_secondary_and_old_variant_results() {
    let mut s = State::default();
    metadata(&mut s, summary(100, "plan", "gateway/astra", Some("low")));
    let held = tuple(&s);
    event(&mut s, 10, "model.changed", change("build", "gateway/old"));
    assert_eq!(tuple(&s), held);
    event(
        &mut s,
        101,
        "model.changed",
        change("build", "gateway/current"),
    );
    let held = tuple(&s);
    event(
        &mut s,
        102,
        "session.agent_changed",
        json!({"agent":"plan"}),
    );
    assert_eq!(tuple(&s), held);
    event(
        &mut s,
        103,
        "command.result",
        json!({"name":"variant","ok":true,"data":{"agent":"plan","model":"gateway/astra","variant":null,"variant_source":"override"}}),
    );
    assert_eq!(tuple(&s), held);
    metadata(&mut s, summary(110, "lux", "gateway/newer", None));
    assert_eq!(s.agent_name(), "lux");
    assert_eq!(s.selected_model_name.as_deref(), Some("gateway/newer"));
    assert_eq!(s.model_seq, Some(110));
}

#[test]
fn archived_root_cannot_resume_prompt_approve_or_continue_historical_watchdog() {
    for promptable in [true, false] {
        let mut s = State::default();
        let mut m = summary(1, "build", "gateway/archive", None);
        m.read_only = true;
        m.promptable = promptable;
        m.resume_disabled_reason = Some("unproven identity".into());
        metadata(&mut s, m);
        s.activity = Activity::Stopped;
        s.watchdog_paused = true;
        let mut ui = UiState::default();
        for action in [
            Action::ResumeSession("S".into()),
            Action::Submit("hi".into()),
            Action::ContinueChild,
            Action::Stop,
            Action::RunCommand {
                name: "model".into(),
                arguments: "gateway/x".into(),
                revision: None,
            },
            Action::AnswerPermission {
                request_id: "R".into(),
                answer: PermissionAnswer {
                    decision: "allow_once".into(),
                    message: None,
                },
            },
        ] {
            assert!(
                reducer::handle_action(&mut s, &mut ui, action)
                    .iter()
                    .all(|e| matches!(e, Effect::SetEditor { .. }))
            );
            assert!(ui.child_confirm.is_none());
        }
        assert!(s.archive());
    }
}

fn agents(s: &mut State, request: u64, session: Option<&str>, default: &str) {
    let body=serde_json::from_value(json!({"default_agent":default,"agents":[{"id":"build","description":"Build","mode":"primary","effective":{"model":"gateway/build","variant":"low","source":"config","variant_source":"model_default"}},{"id":"plan","description":"Plan","mode":"primary","effective":{"model":"gateway/plan","variant":null,"source":"config","variant_source":"model_default"}}]})).unwrap();
    reducer::update(
        s,
        ClientEvent::Agents {
            generation: s.generation.clone(),
            workspace: s.workspace.clone(),
            request,
            session_id: session.map(str::to_string),
            result: Ok(body),
        },
    )
    .unwrap();
}
#[test]
fn explicit_build_survives_plan_default_and_new_discards_start_overrides() {
    let mut s = State::default();
    agents(&mut s, 1, None, "plan");
    let mut ui = UiState::default();
    reducer::handle_action(&mut s, &mut ui, Action::ToggleAgent);
    assert_eq!(s.agent.as_deref(), Some("build"));
    agents(&mut s, 2, None, "plan");
    assert_eq!(s.agent_name(), "build");
    s.start_model = Some("gateway/override".into());
    s.start_variant = Some(None);
    reducer::handle_action(&mut s, &mut ui, Action::NewConversation);
    reducer::handle_action(&mut s, &mut ui, Action::CloseOverlay);
    let fx = reducer::handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
    assert!(
        matches!(&fx[0],Effect::Create(r) if r.agent.is_none() && r.model.is_none() && r.variant.is_none())
    );
}

#[test]
fn workspace_order_and_tab_scoped_agents_reject_foreign_and_reversed_answers() {
    let mut s = State {
        workspace: "/ws".into(),
        ..State::default()
    };
    let g = s.generation.clone();
    for (request, workspace, name) in [
        (2, "/ws", "gateway/new"),
        (1, "/ws", "gateway/old"),
        (3, "/foreign", "gateway/foreign"),
    ] {
        let body=serde_json::from_value(json!({"models":[{"name":name,"gateway":"gateway","upstream_id":"new","api":"chat","provider_group":"gateway","context_window":1}],"default_agent":"build"})).unwrap();
        reducer::update(
            &mut s,
            ClientEvent::Models {
                generation: g.clone(),
                workspace: workspace.into(),
                request,
                result: Ok(body),
            },
        )
        .unwrap();
    }
    assert_eq!(s.catalog[0].name, "gateway/new");
    agents(&mut s, 1, None, "plan");
    for (request, workspace, layer) in [
        (2, "/ws", "reflex"),
        (1, "/ws", "instinct"),
        (3, "/foreign", "metacognition"),
    ] {
        reducer::update(
            &mut s,
            ClientEvent::Layers {
                generation: Generation(99),
                workspace: workspace.into(),
                request,
                result: Ok(LayersBody {
                    layers: vec![LayerEntry {
                        layer: layer.into(),
                        status: "usable".into(),
                        model: Some("gateway/model".into()),
                        variant: None,
                        reason: None,
                    }],
                }),
            },
        )
        .unwrap();
    }
    assert_eq!(s.layers[0].layer, "reflex");
    s.tabs.push(Default::default());
    s.tabs[1].generation = Generation(2);
    s.active = 1;
    let body = serde_json::from_value(json!({"default_agent":"build","agents":[{"id":"lux","description":"Lux","mode":"primary","assignment":null,"effective":null,"warning":null}]})).unwrap();
    reducer::update(
        &mut s,
        ClientEvent::Agents {
            generation: g,
            workspace: "/ws".into(),
            request: 2,
            session_id: None,
            result: Ok(body),
        },
    )
    .unwrap();
    assert!(s.tabs[1].agents.is_empty());
    assert_eq!(s.tabs[0].agents[0].id, "lux");
    // Workspace catalog survives closure of the tab that issued it.
    s.tabs.remove(0);
    s.active = 0;
    let body=serde_json::from_value(json!({"models":[{"name":"gateway/latest","gateway":"gateway","upstream_id":"latest","api":"chat","provider_group":"gateway"}],"default_agent":"build"})).unwrap();
    reducer::update(
        &mut s,
        ClientEvent::Models {
            generation: Generation(0),
            workspace: "/ws".into(),
            request: 4,
            result: Ok(body),
        },
    )
    .unwrap();
    assert_eq!(s.catalog[0].name, "gateway/latest");
}

#[test]
fn variant_only_named_off_and_inherit_keep_creation_tri_state() {
    for (name, expected) in [
        ("high", Some(Some("high".into()))),
        ("off", Some(None)),
        ("inherit", None),
    ] {
        let mut s = State::default();
        let mut ui = UiState {
            variant_pick: Some(VariantPick::Current),
            ..UiState::default()
        };
        reducer::handle_action(&mut s, &mut ui, Action::PickVariant(name.into()));
        assert_eq!(s.start_variant, expected);
        let fx = reducer::handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
        assert!(matches!(&fx[0],Effect::Create(r) if r.model.is_none() && r.variant==expected));
    }
}

#[test]
fn same_model_summary_keeps_committed_window_but_new_model_resolves_its_own() {
    for catalog_window in [None, Some(100000)] {
        let mut s = State {
            catalog: vec![
                ModelEntry {
                    api: withnav_tui::protocol::ApiKind::Chat,
                    name: "gateway/current".into(),
                    context_window: catalog_window,
                    ..Default::default()
                },
                ModelEntry {
                    api: withnav_tui::protocol::ApiKind::Chat,
                    name: "gateway/new".into(),
                    context_window: Some(200000),
                    ..Default::default()
                },
            ],
            ..State::default()
        };
        event(
            &mut s,
            1,
            "model.changed",
            change("build", "gateway/current"),
        );
        metadata(&mut s, summary(10, "build", "gateway/current", None));
        assert_eq!(s.context_window, Some(1000000));
        metadata(&mut s, summary(11, "build", "gateway/new", None));
        assert_eq!(s.context_window, Some(200000));
        assert_eq!(s.selected_model_name.as_deref(), Some("gateway/new"));
    }
}

#[test]
fn real_variant_key_paths_restore_agent_or_model_base_and_omit_inherited_create_variant() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use withnav_tui::input::{Editor, route_key};
    for explicit in [false, true] {
        let mut s = State::default();
        agents(&mut s, 1, None, "build");
        s.catalog = vec![ModelEntry {
            api: withnav_tui::protocol::ApiKind::Chat,
            name: "gateway/build".into(),
            variants: vec!["low".into(), "high".into()],
            default_variant: Some("high".into()),
            ..Default::default()
        }];
        if explicit {
            reducer::handle_action(
                &mut s,
                &mut UiState::default(),
                Action::PickModel("gateway/build".into()),
            );
        }
        let base = if explicit { "high" } else { "low" };
        let mut ui = UiState::default();
        let mut editor = Editor::new();
        for choice in ["off", "inherit", "high", "inherit"] {
            reducer::handle_action(&mut s, &mut ui, Action::OpenOverlay(Overlay::Variants));
            for ch in choice.chars() {
                route_key(
                    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                    &s,
                    &mut ui,
                    &mut editor,
                );
            }
            let action = route_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &s,
                &mut ui,
                &mut editor,
            )
            .unwrap();
            reducer::handle_action(&mut s, &mut ui, action);
            if choice == "inherit" {
                assert_eq!(s.variant.as_deref(), Some(base));
                assert_ne!(s.variant_source, "override");
            }
        }
        let fx = reducer::handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
        assert!(matches!(&fx[0],Effect::Create(r) if r.variant.is_none()));
    }
}

#[test]
fn model_picker_inherit_key_choice_creates_with_omitted_variant() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use withnav_tui::input::{Editor, route_key};
    let mut s = State {
        catalog: vec![ModelEntry {
            api: withnav_tui::protocol::ApiKind::Chat,
            name: "gateway/model".into(),
            gateway: "gateway".into(),
            provider_group: "gateway".into(),
            upstream_id: "model".into(),
            variants: vec!["high".into()],
            default_variant: Some("high".into()),
            ..Default::default()
        }],
        ..State::default()
    };
    let mut ui = UiState::default();
    let mut editor = Editor::new();
    reducer::handle_action(&mut s, &mut ui, Action::OpenOverlay(Overlay::Models));
    route_key(
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    );
    assert!(
        route_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor
        )
        .is_none()
    );
    assert_eq!(ui.overlay, Some(Overlay::Variants));
    for ch in "inherit".chars() {
        route_key(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor,
        );
    }
    let action = route_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    reducer::handle_action(&mut s, &mut ui, action);
    assert_eq!(s.variant.as_deref(), Some("high"));
    let fx = reducer::handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
    let Effect::Create(request) = &fx[0] else {
        panic!("{fx:?}")
    };
    assert!(
        serde_json::to_value(request)
            .unwrap()
            .get("variant")
            .is_none()
    );
}

#[test]
fn archive_header_and_variant_override_view_do_not_claim_child_or_assigned() {
    use ratatui::{Terminal, backend::TestBackend};
    let mut s = State::default();
    let mut m = summary(1, "build", "gateway/archive", None);
    m.read_only = true;
    m.resume_disabled_reason = Some("unproven identity".into());
    metadata(&mut s, m);
    let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
    let mut cache = withnav_tui::view::RenderCache::default();
    t.draw(|f| {
        withnav_tui::view::draw(
            f,
            &s,
            &UiState::default(),
            &withnav_tui::input::Editor::new(),
            &mut cache,
        )
    })
    .unwrap();
    let text = (0..80)
        .map(|x| t.backend().buffer()[(x, 0)].symbol())
        .collect::<String>();
    assert!(text.contains("archive · unproven identity"));
    assert!(!text.contains("subagent"));
    s.session.as_mut().unwrap().read_only = false;
    s.variant_source = "override".into();
    t.draw(|f| {
        withnav_tui::view::draw(
            f,
            &s,
            &UiState::default(),
            &withnav_tui::input::Editor::new(),
            &mut cache,
        )
    })
    .unwrap();
    let text = (0..80)
        .map(|x| t.backend().buffer()[(x, 0)].symbol())
        .collect::<String>();
    assert!(text.contains("override"), "{text}");
}
