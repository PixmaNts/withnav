mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use navai::tui::{
    client::Client,
    config::*,
    input::{route_key, Editor},
    protocol::{Action, ApiKind, ClientEvent, Effect, Generation, ModelEntry},
    reducer,
    state::{State, UiState},
    theme::ThemeId,
};
use serde_json::{json, Value};

fn body(revision: &str) -> SettingsBody {
    serde_json::from_value(json!({"revision": revision, "paths": {"config":"/cfg/config.json","models":null,"agents":[]}, "gateways":[], "fields": [
        {"key":"layers.reflex","value":null,"default":null,"set":false,"error":null,"applies":"restart"},
        {"key":"permissions.automatic_review","value":true,"default":true,"set":true,"error":null,"applies":"new_sessions"},
        {"key":"permissions.scratch_dirs","value":[],"default":[],"set":true,"error":null,"applies":"new_sessions"},
        {"key":"subagents.max_concurrent","value":4,"default":4,"set":true,"error":null,"applies":"new_sessions"},
        {"key":"tui.theme","value":"ayu-dark","default":"ayu-dark","set":true,"error":null,"applies":"live"}
    ]})).unwrap()
}
fn setup() -> (State, UiState) {
    let mut state = State::default();
    state.config.body = Some(body("r0"));
    (
        state,
        UiState {
            screen: Screen::Config,
            ..Default::default()
        },
    )
}
fn action(s: &mut State, ui: &mut UiState, a: ConfigAction) -> Vec<Effect> {
    reducer::handle_action(s, ui, Action::Config(a))
}
fn response(status: &str, body: Value) -> Vec<u8> {
    let text = body.to_string();
    format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).into_bytes()
}

#[test]
fn navigate_jump_stage_revert() {
    let (mut s, mut ui) = setup();
    let mut editor = Editor::default();
    let a = route_key(
        KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    reducer::handle_action(&mut s, &mut ui, a);
    assert_eq!(s.config.section, Section::Permissions);
    action(&mut s, &mut ui, ConfigAction::Move(1));
    assert_eq!(s.config.row, 1);
    action(&mut s, &mut ui, ConfigAction::CycleSection(1));
    assert_eq!(s.config.section, Section::Subagents);
    action(&mut s, &mut ui, ConfigAction::Step(1));
    assert_eq!(s.config.staged["subagents.max_concurrent"], json!(5));
    action(&mut s, &mut ui, ConfigAction::Revert);
    assert!(s.config.staged.is_empty());
}

#[test]
fn leave_prompt_write_discard_stay() {
    for choice in [LeaveChoice::Stay, LeaveChoice::Discard, LeaveChoice::Write] {
        let (mut s, mut ui) = setup();
        s.config.stage("tui.theme".into(), json!("nord-dark"));
        action(&mut s, &mut ui, ConfigAction::Leave);
        assert!(s.config.leave_prompt);
        let effects = action(&mut s, &mut ui, ConfigAction::ResolveLeave(choice.clone()));
        assert!(!s.config.leave_prompt);
        match choice {
            LeaveChoice::Stay => {
                assert_eq!(ui.screen, Screen::Config);
                assert_eq!(s.config.staged.len(), 1);
            }
            LeaveChoice::Discard => {
                assert_eq!(ui.screen, Screen::Session);
                assert!(s.config.staged.is_empty());
            }
            LeaveChoice::Write => {
                assert!(matches!(effects[0], Effect::WriteConfig(_)));
                assert!(s.config.leave_after_write);
            }
        }
    }
}

#[test]
fn picker_filters_api_and_stages_assignment() {
    let (mut s, mut ui) = setup();
    s.catalog = [ApiKind::Chat, ApiKind::Fim, ApiKind::SystemOne]
        .into_iter()
        .map(|api| ModelEntry {
            name: format!("g/{api:?}"),
            api,
            gateway: "g".into(),
            provider_group: "g".into(),
            ..Default::default()
        })
        .collect();
    action(&mut s, &mut ui, ConfigAction::Jump(Section::Layers));
    action(&mut s, &mut ui, ConfigAction::Edit);
    assert_eq!(ui.settings_target.as_ref().unwrap().api, ApiKind::Fim);
    let names: Vec<_> = navai::tui::catalog::rows(&s.catalog, &ui)
        .iter()
        .filter_map(|r| match r {
            navai::tui::catalog::Row::Model(m) => Some(m.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["g/Fim"]);
    action(&mut s, &mut ui, ConfigAction::PickModel("g/Fim".into()));
    assert_eq!(
        s.config.staged["layers.reflex"],
        json!({"model":"g/Fim","variant":null})
    );
    assert!(s.session.is_none());
}

#[tokio::test]
async fn write_carries_revision_and_theme_applies_live() {
    let server = common::sse::spawn_capturing_provider(vec![
        response("200 OK", serde_json::to_value(body("r1")).unwrap()),
        response("200 OK", {
            let mut b = body("r2");
            b.fields.last_mut().unwrap().value = json!("nord-dark");
            serde_json::to_value(b).unwrap()
        }),
    ])
    .await;
    let client = Client::new(server.url.trim_end_matches("/v1"), None);
    let (mut s, mut ui) = setup();
    s.config
        .stage("permissions.automatic_review".into(), json!(false));
    s.config.stage("tui.theme".into(), json!("nord-dark"));
    let mut editor = Editor::default();
    let a = route_key(
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    let mut effects = reducer::handle_action(&mut s, &mut ui, a);
    for _ in 0..2 {
        let Effect::WriteConfig(request) = effects.remove(0) else {
            panic!("write")
        };
        let result = client.write_setting("/workspace", &request).await;
        effects = reducer::update(
            &mut s,
            ClientEvent::ConfigWritten {
                generation: Generation(0),
                key: request.key,
                value: request.value,
                result,
            },
        )
        .unwrap();
    }
    assert_eq!(effects, vec![Effect::ApplyConfigTheme(ThemeId::NordDark)]);
    assert_eq!(s.config.status, SaveStatus::Written);
    let requests: Vec<Value> = server
        .bodies()
        .iter()
        .map(|b| serde_json::from_str(b).unwrap())
        .collect();
    assert_eq!(requests[0]["revision"], "r0");
    assert_eq!(requests[1]["revision"], "r1");
    let mut runtime = navai::tui::runtime::Runtime::new(&navai::tui::TuiOptions {
        url: client.base_url,
        token: None,
        workspace: "/workspace".into(),
        initial_model: None,
        initial_session: None,
        embedded: false,
        config_dir: None,
    });
    runtime.run_effects(effects);
    assert_eq!(runtime.ui.theme, ThemeId::NordDark);
}

#[tokio::test]
async fn stale_reloads_keeps_unsent_and_invalid_marks_row() {
    for stale in [true, false] {
        let error = if stale {
            json!({"reason":"stale","error":"changed"})
        } else {
            json!({"reason":"invalid","key":"tui.theme","error":"unknown theme"})
        };
        let server = common::sse::spawn_capturing_provider(vec![
            response(
                if stale {
                    "409 Conflict"
                } else {
                    "400 Bad Request"
                },
                error,
            ),
            response("200 OK", serde_json::to_value(body("new")).unwrap()),
        ])
        .await;
        let client = Client::new(server.url.trim_end_matches("/v1"), None);
        let (mut s, mut ui) = setup();
        s.config.stage("tui.theme".into(), json!("bad"));
        let Effect::WriteConfig(request) = action(&mut s, &mut ui, ConfigAction::Write).remove(0)
        else {
            panic!("write")
        };
        let result = client.write_setting("/workspace", &request).await;
        let effects = written(&mut s.config, request.key, request.value, result);
        assert_eq!(s.config.staged.len(), 1);
        if stale {
            assert_eq!(effects, vec![Effect::FetchConfig]);
            loaded(&mut s.config, client.settings("/workspace").await);
            assert_eq!(s.config.status, SaveStatus::StaleReloaded(1));
        } else {
            assert!(effects.is_empty());
            assert_eq!(s.config.field_errors["tui.theme"], "unknown theme");
        }
    }
}

#[test]
fn inline_scratch_paths_require_absolute_paths() {
    let (mut s, mut ui) = setup();
    action(
        &mut s,
        &mut ui,
        ConfigAction::ListText("relative\n/tmp/valid".into()),
    );
    action(&mut s, &mut ui, ConfigAction::CommitList);
    assert!(s.config.staged.is_empty());
    assert!(s.config.list_editor.is_some());
    action(
        &mut s,
        &mut ui,
        ConfigAction::ListText("/tmp/one\n/tmp/two".into()),
    );
    action(&mut s, &mut ui, ConfigAction::CommitList);
    assert_eq!(
        s.config.staged["permissions.scratch_dirs"],
        json!(["/tmp/one", "/tmp/two"])
    );
}
