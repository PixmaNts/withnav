mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};
use withnav::tui::{
    client::Client,
    config::*,
    input::{route_key, Editor},
    protocol::{Action, ApiKind, ClientEvent, Effect, Generation, ModelEntry},
    reducer,
    state::{State, UiState},
    theme::ThemeId,
};

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
    let names: Vec<_> = withnav::tui::catalog::rows(&s.catalog, &ui)
        .iter()
        .filter_map(|r| match r {
            withnav::tui::catalog::Row::Model(m) => Some(m.name.clone()),
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
    let mut runtime = withnav::tui::runtime::Runtime::new(&withnav::tui::TuiOptions {
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
            let [Effect::FetchConfig(request)] = effects.as_slice() else {
                panic!("reload")
            };
            loaded(&mut s.config, *request, client.settings("/workspace").await);
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

fn load_request(effect: Effect) -> u64 {
    let Effect::FetchConfig(request) = effect else {
        panic!("expected settings fetch")
    };
    request
}

fn deliver_load(s: &mut State, request: u64, result: Result<SettingsBody, String>) -> Vec<Effect> {
    reducer::update(
        s,
        ClientEvent::ConfigLoaded {
            generation: Generation(0),
            request,
            result,
        },
    )
    .unwrap()
}

#[test]
fn late_get_after_successful_put_keeps_written_value() {
    let (mut s, mut ui) = setup();
    s.config.stage("tui.theme".into(), json!("nord-dark"));
    let Effect::WriteConfig(write) = action(&mut s, &mut ui, ConfigAction::Write).remove(0) else {
        panic!("write")
    };
    // Reopening can fetch while an earlier write is still completing.
    let request =
        load_request(reducer::handle_action(&mut s, &mut ui, Action::OpenConfig).remove(0));
    let mut saved = body("r1");
    saved.fields.last_mut().unwrap().value = json!("nord-dark");
    reducer::update(
        &mut s,
        ClientEvent::ConfigWritten {
            generation: Generation(0),
            key: write.key,
            value: write.value,
            result: Ok(saved.clone()),
        },
    )
    .unwrap();
    assert!(deliver_load(&mut s, request, Ok(body("r0"))).is_empty());
    assert_eq!(s.config.body, Some(saved));
    assert_eq!(s.config.value("tui.theme"), json!("nord-dark"));
    assert_eq!(s.config.status, SaveStatus::Written);
    s.config.stage("subagents.max_concurrent".into(), json!(5));
    let Effect::WriteConfig(write) = action(&mut s, &mut ui, ConfigAction::Write).remove(0) else {
        panic!("write")
    };
    assert_eq!(write.revision, "r1");
}

#[test]
fn out_of_order_gets_keep_newest_and_block_writes_while_loading() {
    let (mut s, mut ui) = setup();
    s.config.stage("tui.theme".into(), json!("nord-dark"));
    let old = load_request(reducer::handle_action(&mut s, &mut ui, Action::OpenConfig).remove(0));
    let new = load_request(reducer::handle_action(&mut s, &mut ui, Action::OpenConfig).remove(0));
    assert!(action(&mut s, &mut ui, ConfigAction::Write).is_empty());
    assert!(s.config.writing.is_none());
    deliver_load(&mut s, old, Ok(body("old")));
    assert_eq!(s.config.loading, Some(new));
    assert!(action(&mut s, &mut ui, ConfigAction::Write).is_empty());
    deliver_load(&mut s, new, Ok(body("new")));
    deliver_load(&mut s, old, Ok(body("old")));
    assert_eq!(s.config.body.as_ref().unwrap().revision, "new");
    let Effect::WriteConfig(write) = action(&mut s, &mut ui, ConfigAction::Write).remove(0) else {
        panic!("write")
    };
    assert_eq!(write.revision, "new");
}

#[test]
fn failed_stale_reload_ctrl_s_retries_get_and_keeps_staged_keys() {
    let (mut s, mut ui) = setup();
    s.config.stage("tui.theme".into(), json!("nord-dark"));
    s.config.stage("subagents.max_concurrent".into(), json!(5));
    let staged = s.config.staged.clone();
    let Effect::WriteConfig(write) = action(&mut s, &mut ui, ConfigAction::Write).remove(0) else {
        panic!("write")
    };
    let request = load_request(
        written(
            &mut s.config,
            write.key,
            write.value,
            Err(SettingsError::Stale),
        )
        .remove(0),
    );
    deliver_load(&mut s, request, Err("reload unavailable".into()));
    assert!(
        matches!(&s.config.status, SaveStatus::Error { message, .. } if message == "reload unavailable")
    );
    assert!(s.config.stale_reload);
    assert!(s.config.loading.is_none());
    let mut editor = Editor::default();
    let key = route_key(
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    let retry = load_request(reducer::handle_action(&mut s, &mut ui, key).remove(0));
    assert!(retry > request);
    assert_eq!(s.config.staged, staged);
    assert!(action(&mut s, &mut ui, ConfigAction::Write).is_empty());
    assert!(s.config.writing.is_none());
    deliver_load(&mut s, retry, Ok(body("fresh")));
    assert!(!s.config.stale_reload);
    assert_eq!(s.config.staged, staged);
    let Effect::WriteConfig(write) = action(&mut s, &mut ui, ConfigAction::Write).remove(0) else {
        panic!("write")
    };
    assert_eq!(write.revision, "fresh");
}

#[test]
fn failed_stale_reload_leave_write_retries_before_put() {
    let (mut s, mut ui) = setup();
    s.config.stage("tui.theme".into(), json!("nord-dark"));
    s.config.stale_reload = true;
    let request = load_request(s.config.fetch());
    deliver_load(&mut s, request, Err("unavailable".into()));
    action(&mut s, &mut ui, ConfigAction::Leave);
    let retry = load_request(
        action(
            &mut s,
            &mut ui,
            ConfigAction::ResolveLeave(LeaveChoice::Write),
        )
        .remove(0),
    );
    assert!(s.config.writing.is_none());
    let Effect::WriteConfig(write) = deliver_load(&mut s, retry, Ok(body("fresh"))).remove(0)
    else {
        panic!("write after reload")
    };
    assert_eq!(write.revision, "fresh");
    assert_eq!(write.value, json!("nord-dark"));
    assert!(
        written(&mut s.config, write.key, write.value, Ok(body("saved")))
            .contains(&Effect::LeaveConfig)
    );
}

#[test]
fn scratch_list_route_key_enter_adds_newline_ctrl_s_stages() {
    let (mut s, mut ui) = setup();
    let mut editor = Editor::default();
    action(&mut s, &mut ui, ConfigAction::Jump(Section::Permissions));
    action(&mut s, &mut ui, ConfigAction::Move(1));
    for code in std::iter::once(KeyCode::Enter)
        .chain("/tmp/one".chars().map(KeyCode::Char))
        .chain(std::iter::once(KeyCode::Enter))
        .chain("/tmp/two".chars().map(KeyCode::Char))
    {
        let key = route_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            &s,
            &mut ui,
            &mut editor,
        )
        .unwrap();
        reducer::handle_action(&mut s, &mut ui, key);
    }
    assert_eq!(s.config.list_editor.as_deref(), Some("/tmp/one\n/tmp/two"));
    assert!(s.config.staged.is_empty());
    let key = route_key(
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        &s,
        &mut ui,
        &mut editor,
    )
    .unwrap();
    assert!(reducer::handle_action(&mut s, &mut ui, key).is_empty());
    assert!(s.config.list_editor.is_none());
    assert_eq!(
        s.config.staged["permissions.scratch_dirs"],
        json!(["/tmp/one", "/tmp/two"])
    );
    assert!(s.config.writing.is_none());
}

#[test]
fn stay_and_discard_cancel_pending_leave_write_and_discard_clears_row_errors() {
    for choice in [LeaveChoice::Stay, LeaveChoice::Discard] {
        let (mut s, mut ui) = setup();
        s.config.stage("tui.theme".into(), json!("nord-dark"));
        s.config
            .field_errors
            .insert("tui.theme".into(), "not saved".into());
        s.config
            .field_errors
            .insert("layers.reflex".into(), "unrelated error".into());
        let request = load_request(s.config.fetch());
        action(&mut s, &mut ui, ConfigAction::Leave);
        assert!(action(
            &mut s,
            &mut ui,
            ConfigAction::ResolveLeave(LeaveChoice::Write)
        )
        .is_empty());
        assert!(s.config.leave_after_write);
        assert!(s.config.writing.is_none());
        action(&mut s, &mut ui, ConfigAction::Leave);
        action(&mut s, &mut ui, ConfigAction::ResolveLeave(choice.clone()));
        assert!(!s.config.leave_after_write);
        assert!(deliver_load(&mut s, request, Ok(body("fresh"))).is_empty());
        assert!(s.config.writing.is_none());
        match choice {
            LeaveChoice::Stay => {
                assert_eq!(ui.screen, Screen::Config);
                assert_eq!(s.config.staged["tui.theme"], json!("nord-dark"));
                assert!(s.config.field_errors.contains_key("tui.theme"));
            }
            LeaveChoice::Discard => {
                assert_eq!(ui.screen, Screen::Session);
                assert!(s.config.staged.is_empty());
                assert!(!s.config.field_errors.contains_key("tui.theme"));
                assert_eq!(s.config.field_errors["layers.reflex"], "unrelated error");
            }
            LeaveChoice::Write => unreachable!(),
        }
    }
}

#[tokio::test]
async fn runtime_routes_y_to_settings_scratch_editor_and_filter_not_transcript_selection() {
    use crossterm::event::Event;
    use withnav::tui::{runtime::Runtime, state::Focus, TuiOptions};
    let mut runtime = Runtime::new(&TuiOptions {
        url: "http://127.0.0.1:1".into(),
        token: None,
        workspace: "/workspace".into(),
        initial_model: None,
        initial_session: None,
        embedded: false,
        config_dir: None,
    });
    runtime.state.config.body = Some(body("r0"));
    runtime.ui.screen = Screen::Config;
    runtime.ui.focus = Focus::Transcript;
    let press = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    runtime.handle_terminal_event(press(KeyCode::Char('4')));
    runtime.handle_terminal_event(press(KeyCode::Down));
    runtime.handle_terminal_event(press(KeyCode::Enter));
    for c in "/tmp/y".chars() {
        runtime.handle_terminal_event(press(KeyCode::Char(c)));
    }
    assert_eq!(runtime.state.config.list_editor.as_deref(), Some("/tmp/y"));
    runtime.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(
        runtime.state.config.staged["permissions.scratch_dirs"],
        json!(["/tmp/y"])
    );
    runtime.handle_terminal_event(press(KeyCode::Char('/')));
    runtime.handle_terminal_event(press(KeyCode::Char('y')));
    assert_eq!(runtime.state.config.filter, "y");
    assert!(runtime.state.notice.is_none());
}
