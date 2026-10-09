//! Reducer tests over JSONL fixtures built from the crate's real event types.
//! Regenerate the fixtures with
//! `cargo test --test tui_reducer -- --ignored regenerate_fixtures`.
use serde::Serialize;
use serde_json::{json, Value};
use ulid::Ulid;
use withnav::tui::protocol::{ContextSnapshot, ModelIdentity};
use withnav::{
    bus::event::*,
    provider::{AssistantMessage, StreamDelta, ToolCall, Usage},
    tools::ToolStatus,
    tui::{
        protocol::*,
        reducer::{handle_action, update},
        state::*,
    },
};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tui");

#[test]
fn session_delete_requires_confirmation_and_closes_parent_and_child_tabs() {
    let mut s = subagent_parent();
    let root = s.session.as_ref().unwrap().session_id.clone();
    let entry: SessionListEntry = serde_json::from_value(json!({
        "id":root,"created_at":"today","updated_at":"today","message_count":0,"live":true
    }))
    .unwrap();
    let mut child_entry = entry.clone();
    child_entry.id = "delete-child".into();
    child_entry.parent_session_id = Some(root.clone());
    s.sessions = vec![entry, child_entry];
    let mut child_tab = s.tabs[0].clone();
    child_tab.session.as_mut().unwrap().session_id = "delete-child".into();
    child_tab.parent_session_id = Some(root.clone());
    s.tabs.push(child_tab);
    let mut ui = UiState {
        overlay: Some(Overlay::Sessions),
        ..Default::default()
    };
    assert!(handle_action(&mut s, &mut ui, Action::RequestDeleteSession(root.clone())).is_empty());
    handle_action(&mut s, &mut ui, Action::CancelDeleteSession);
    assert!(handle_action(&mut s, &mut ui, Action::ConfirmDeleteSession).is_empty());
    handle_action(&mut s, &mut ui, Action::RequestDeleteSession(root.clone()));
    assert_eq!(
        handle_action(&mut s, &mut ui, Action::ConfirmDeleteSession),
        vec![Effect::DeleteSession(root.clone())]
    );
    let effects = handle_action(
        &mut s,
        &mut ui,
        Action::SessionDeleted {
            session_id: root,
            result: Ok(true),
        },
    );
    assert!(effects.contains(&Effect::ListSessions));
    assert!(s.sessions.is_empty());
    assert_eq!(s.tabs.len(), 1);
    assert!(s.session.is_none());
    assert!(ui.delete_pending.is_none());
    assert_eq!(ui.overlay, Some(Overlay::Sessions));
}

#[test]
fn selected_skills_accept_current_and_legacy_human_origins() {
    for origin in ["captain", "driver"] {
        let mut s = st();
        subagent_event(
            &mut s,
            1,
            "message.received",
            json!({
                "source": origin, "content": "use the skill",
                "selected_skill": {"origin": origin, "name": "rust"}
            }),
        );
        assert!(s.conversation.blocks.iter().any(|block| matches!(
            &block.kind, BlockKind::Notice(text) if text == "skill: rust · selected by Captain"
        )));
    }
}

#[test]
fn bypass_is_acknowledged_pending_and_scoped_to_the_tab() {
    let mut s = subagent_parent();
    let mut ui = UiState::default();
    assert!(matches!(
        handle_action(&mut s, &mut ui, Action::ToggleBypass).as_slice(),
        [Effect::SetBypass { enabled: true, .. }]
    ));
    assert!(!s.bypass);
    assert!(handle_action(&mut s, &mut ui, Action::ToggleBypass).is_empty());
    let generation = s.generation.clone();
    update(
        &mut s,
        ClientEvent::BypassSet {
            generation,
            result: Ok(BypassResponse {
                enabled: true,
                changed: true,
                bypass_seq: 42,
            }),
        },
    )
    .unwrap();
    assert!(s.bypass);
    assert!(!s.bypass_pending);
    let mut script = Script::new();
    script.raw("session.bypass", json!({"enabled":false,"by":"user:http"}));
    for envelope in script.out {
        let generation = s.generation.clone();
        update(
            &mut s,
            ClientEvent::Envelope {
                generation,
                envelope,
            },
        )
        .unwrap();
    }
    assert!(s.bypass);
    assert!(
        matches!(s.conversation.blocks.last().map(|b| &b.kind), Some(BlockKind::Notice(text)) if text == "bypass off")
    );
    handle_action(&mut s, &mut ui, Action::NewConversation);
    assert!(!s.bypass);
    assert!(s.tabs.iter().any(|tab| tab.bypass));
}

#[test]
fn bypass_setters_are_disabled_for_child_and_archive_views() {
    let mut s = subagent_parent();
    let mut ui = UiState::default();
    s.parent_session_id = Some("parent".into());
    assert!(handle_action(&mut s, &mut ui, Action::ToggleBypass).is_empty());
    s.parent_session_id = None;
    s.session.as_mut().unwrap().read_only = true;
    assert!(handle_action(&mut s, &mut ui, Action::ToggleBypass).is_empty());
}

#[test]
fn bypass_snapshot_watermark_prevents_replayed_on_and_end_clears_live_state() {
    let mut s = subagent_parent();
    let mut metadata = s.session.clone().unwrap();
    metadata.bypass = false;
    metadata.bypass_seq = 10;
    let generation = s.generation.clone();
    update(
        &mut s,
        ClientEvent::Metadata {
            generation,
            result: Ok(metadata),
        },
    )
    .unwrap();
    let mut script = Script::new();
    script.raw("session.bypass", json!({"enabled":true,"by":"user:http"}));
    for envelope in script.out {
        let generation = s.generation.clone();
        update(
            &mut s,
            ClientEvent::Envelope {
                generation,
                envelope,
            },
        )
        .unwrap();
    }
    assert!(!s.bypass);
    let mut e = EventEnvelope::new(
        Ulid::new(),
        11,
        EventKind::SessionEnd,
        json!({"reason":"stopped"}),
    );
    s.bypass = true;
    e.actor_id = "agent:main".into();
    let generation = s.generation.clone();
    update(
        &mut s,
        ClientEvent::Envelope {
            generation,
            envelope: e,
        },
    )
    .unwrap();
    assert!(!s.bypass);
    assert_eq!(s.bypass_seq, 11);
}

#[test]
fn root_watchdog_notice_and_pause_render_and_continue_is_not_an_ask_answer() {
    let mut s = subagent_parent();
    let mut ui = UiState::default();
    let mut script = Script::new();
    script.raw("command.result", json!({"name":"watchdog","ok":true,"data":{"message":"Stopped by watchdog: looping; partial work kept"}}));
    script.raw(
        "agent.yield",
        json!({"kind":"watchdog","question":"Paused: alt+c continue or Stop"}),
    );
    for envelope in script.out {
        let generation = s.generation.clone();
        update(
            &mut s,
            ClientEvent::Envelope {
                generation,
                envelope,
            },
        )
        .unwrap();
    }
    assert!(
        matches!(&s.conversation.blocks[0].kind, BlockKind::Notice(text) if text.contains("Stopped by watchdog"))
    );
    assert!(
        matches!(&s.conversation.blocks[1].kind, BlockKind::Notice(text) if text.contains("Watchdog pause") && text.contains("alt+c"))
    );
    assert!(s.watchdog_paused);
    assert_eq!(s.activity, Activity::AwaitingUser);
    handle_action(&mut s, &mut ui, Action::ContinueChild);
    assert_eq!(
        ui.child_confirm
            .as_ref()
            .map(|(parent, child, _)| (parent.clone(), child.clone())),
        Some((Ulid(1).to_string(), Ulid(1).to_string()))
    );
    assert!(
        matches!(handle_action(&mut s, &mut ui, Action::ConfirmChildControl).as_slice(), [Effect::ControlChild { parent_session_id, child_session_id, .. }] if parent_session_id == child_session_id)
    );
    handle_action(&mut s, &mut ui, Action::ContinueChild);
    s.watchdog_paused = false;
    assert!(handle_action(&mut s, &mut ui, Action::ConfirmChildControl).is_empty());
    handle_action(&mut s, &mut ui, Action::ContinueChild);
    assert!(ui.child_confirm.is_none());
}

#[test]
fn budget_notices_name_the_root_or_child_continue_path() {
    for child in [false, true] {
        let mut s = subagent_parent();
        if child {
            s.parent_session_id = Some(Ulid(2).to_string());
        }
        let mut script = Script::new();
        script.raw(
            "agent.yield",
            json!({"kind":"budget","question":"step budget exhausted"}),
        );
        let generation = s.generation.clone();
        update(
            &mut s,
            ClientEvent::Envelope {
                generation,
                envelope: script.out.remove(0),
            },
        )
        .unwrap();
        assert!(
            matches!(s.conversation.blocks.last().map(|b| &b.kind),Some(BlockKind::Notice(text)) if text.contains(if child {"alt+c"} else {"send a message"}))
        );
    }
}

#[test]
fn pause_lifecycle_replay_expiry_reset_and_late_continue_are_not_actionable() {
    let first = PauseToken {
        incarnation: Ulid(10),
        hold_id: Ulid(11),
    };
    let second = PauseToken {
        incarnation: Ulid(10),
        hold_id: Ulid(12),
    };
    let mut s = subagent_parent();
    let mut script = Script::new();
    script.raw(
        "agent.yield",
        json!({"kind":"watchdog","question":"paused","details":{"pause_token":first}}),
    );
    script.raw("command.result", json!({"name":"watchdog","ok":true,"data":{"pause_token":first,"resolution":"expired","message":"expired"}}));
    for envelope in script.out {
        let generation = s.generation.clone();
        update(
            &mut s,
            ClientEvent::Envelope {
                generation,
                envelope,
            },
        )
        .unwrap();
    }
    assert!(!s.watchdog_paused);
    assert!(s.pause_token.is_none());
    s.watchdog_paused = true;
    s.pause_token = Some(second);
    let generation = s.generation.clone();
    update(
        &mut s,
        ClientEvent::ChildControlled {
            generation,
            child_session_id: Ulid(1).to_string(),
            control: ChildControl::Continue,
            result: Ok(()),
            pause_token: Some(first),
        },
    )
    .unwrap();
    assert_eq!(s.pause_token, Some(second));
    assert!(s.watchdog_paused);
    let mut ui = UiState::default();
    handle_action(&mut s, &mut ui, Action::ContinueChild);
    s.pause_token = Some(first);
    assert!(handle_action(&mut s, &mut ui, Action::ConfirmChildControl).is_empty());
    let generation = s.generation.clone();
    update(&mut s, ClientEvent::Resync { generation }).unwrap();
    assert!(!s.watchdog_paused);
    assert!(s.pause_token.is_none());
}

#[test]
fn interrupted_steering_keeps_watched_and_open_children_running_then_parked() {
    for opened in [false, true] {
        for close_parent in [false, true] {
            if !opened && close_parent {
                continue;
            }
            let mut s = subagent_parent();
            spawn_subagent(&mut s, None);
            let mut ui = UiState::default();
            if opened {
                handle_action(&mut s, &mut ui, Action::OpenChild(Ulid(2).to_string()));
                let metadata = serde_json::from_value(json!({"session_id":Ulid(2),"workspace":"/ws","model":"fast","actor_state":"active","agent":"vigie","current_seq":0,"parent_session_id":Ulid(1),"promptable":false,"child_status":"running"})).unwrap();
                let generation = s.generation.clone();
                update(
                    &mut s,
                    ClientEvent::Metadata {
                        generation,
                        result: Ok(metadata),
                    },
                )
                .unwrap();
                if close_parent {
                    s.active = 0;
                    handle_action(&mut s, &mut ui, Action::CloseTab);
                }
            }
            let mut script = Script::new();
            script.seq = 10;
            script.raw("turn.end", json!({"outcome":"interrupted"}));
            script.raw("turn.start", json!({"turn_id":"steered-turn"}));
            script.raw(
                "agent.yield",
                json!({"kind":"budget","question":"step budget exhausted"}),
            );
            for replay in [false, true] {
                if replay && opened {
                    let generation = s.generation.clone();
                    update(&mut s, ClientEvent::Resync { generation }).unwrap();
                }
                for (index, envelope) in script.out.iter().enumerate() {
                    let generation = s.generation.clone();
                    let event = if opened {
                        ClientEvent::Envelope {
                            generation,
                            envelope: envelope.clone(),
                        }
                    } else {
                        ClientEvent::ChildEnvelope {
                            generation,
                            child_session_id: Ulid(2).to_string(),
                            envelope: envelope.clone(),
                        }
                    };
                    let effects = update(&mut s, event).unwrap();
                    assert!(!effects.iter().any(|e| matches!(
                        e,
                        Effect::FetchChildTerminalMetadata(_) | Effect::UnwatchChild(_)
                    )));
                    let status = if opened {
                        s.active_child_status()
                    } else {
                        s.children[0].status
                    };
                    assert!(!status.terminal());
                    if !replay {
                        assert_eq!(
                            status,
                            if index == 2 {
                                ChildStatus::OutOfSteps
                            } else {
                                ChildStatus::Running
                            }
                        );
                    }
                }
                if opened {
                    handle_action(&mut s, &mut ui, Action::ContinueChild);
                    assert!(ui.child_confirm.is_some());
                    handle_action(&mut s, &mut ui, Action::CancelChildControl);
                } else {
                    assert_eq!(s.children[0].status, ChildStatus::OutOfSteps);
                }
            }
        }
    }
}

#[test]
fn a_child_finishes_after_its_parent_closes_and_refreshes_its_outcome() {
    for (kind, payload, expected) in [
        ("turn.end", json!({"outcome":"done"}), ChildStatus::Done),
        ("turn.end", json!({"outcome":"error"}), ChildStatus::Failed),
        (
            "session.end",
            json!({"reason":"stopped_by_owner"}),
            ChildStatus::Stopped,
        ),
    ] {
        let mut s = subagent_parent();
        spawn_subagent(&mut s, None);
        let mut ui = UiState::default();
        handle_action(&mut s, &mut ui, Action::OpenChild(Ulid(2).to_string()));
        s.active = 0;
        handle_action(&mut s, &mut ui, Action::CloseTab);
        assert_eq!(s.tabs.len(), 1);
        let effects = subagent_event(&mut s, 10, kind, payload);
        let projected = if kind == "session.end" {
            expected
        } else {
            ChildStatus::Running
        };
        assert_eq!(s.active_child_status(), projected);
        assert!(effects.contains(&Effect::FetchChildTerminalMetadata(Ulid(2).to_string())));
        assert!(
            !subagent_event(&mut s, 10, "turn.start", json!({"turn_id":"late"}))
                .contains(&Effect::FetchChildTerminalMetadata(Ulid(2).to_string()))
        );
        assert_eq!(s.active_child_status(), projected);
        subagent_event(&mut s, 11, "turn.start", json!({"turn_id":"late"}));
        assert_eq!(s.active_child_status(), projected);
        let metadata: SessionMetadata = serde_json::from_value(json!({"session_id":Ulid(2),"workspace":"/ws","model":"fast","agent":"vigie","actor_state":"stopped","current_seq":11,"parent_session_id":Ulid(1),"promptable":false,"child_status":expected.label()})).unwrap();
        let generation = s.generation.clone();
        update(
            &mut s,
            ClientEvent::Metadata {
                generation,
                result: Ok(metadata),
            },
        )
        .unwrap();
        assert_eq!(s.active_child_status(), expected);
    }
}

#[tokio::test]
async fn terminal_metadata_refresh_retries_until_the_persisted_outcome_arrives() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };
    use withnav::tui::{
        client::Client,
        input::Editor,
        runtime::{Context, Executor},
    };
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    Mock::given(method("GET")).and(path("/v1/sessions/child"))
        .respond_with(move |_: &wiremock::Request| {
            let status = if count.fetch_add(1, Ordering::SeqCst) == 0 { "running" } else { "done" };
            ResponseTemplate::new(200).set_body_json(json!({"session_id":"child","workspace":"/ws","model":"fast","actor_state":"stopped","current_seq":10,"parent_session_id":"parent","promptable":false,"child_status":status}))
        }).expect(2).mount(&server).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut executor = Executor::new(Client::new(server.uri(), None), tx);
    executor.execute(
        Effect::FetchChildTerminalMetadata("child".into()),
        &Context {
            generation: Generation(7),
            session_id: Some("child".into()),
            workspace: "/ws".into(),
            config_dir: None,
        },
        &mut Editor::new(),
    );
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(event, ClientEvent::Metadata { generation:Generation(7), result:Ok(m) } if m.child_status.as_deref() == Some("done"))
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn old_initialization_never_overwrites_ordered_or_terminal_child_facts() {
    let mut s = subagent_parent();
    let entry: SessionListEntry = serde_json::from_value(json!({"id":Ulid(2),"created_at":"now","updated_at":"now","message_count":0,"live":true,"agent":"vigie","parent_session_id":Ulid(1),"child_status":"running","promptable":false})).unwrap();
    let list = ClientEvent::Sessions {
        generation: Generation(0),
        result: Ok(SessionListBody {
            sessions: vec![entry.clone()],
        }),
    };
    update(&mut s, list.clone()).unwrap();
    let mut script = Script::new();
    script.seq = 10;
    script.raw("agent.yield", json!({"kind":"budget"}));
    update(
        &mut s,
        ClientEvent::ChildEnvelope {
            generation: Generation(0),
            child_session_id: Ulid(2).to_string(),
            envelope: script.out[0].clone(),
        },
    )
    .unwrap();
    update(&mut s, list.clone()).unwrap();
    assert_eq!(s.children[0].status, ChildStatus::OutOfSteps);
    spawn_subagent(&mut s, None);
    assert_eq!(s.children[0].status, ChildStatus::OutOfSteps);
    let mut terminal = entry;
    terminal.child_status = Some("done".into());
    update(
        &mut s,
        ClientEvent::Sessions {
            generation: Generation(0),
            result: Ok(SessionListBody {
                sessions: vec![terminal],
            }),
        },
    )
    .unwrap();
    update(&mut s, list).unwrap();
    subagent_event(
        &mut s,
        2,
        "subagent.spawned",
        json!({"parent_session_id":Ulid(1),"child_session_id":Ulid(2),"agent":"vigie","goal":"Read tests"}),
    );
    assert_eq!(s.children[0].status, ChildStatus::Done);
}

#[test]
fn parent_resync_keeps_a_parked_child_and_its_continue_control() {
    let mut s = subagent_parent();
    spawn_subagent(&mut s, None);
    let mut script = Script::new();
    script.seq = 10;
    script.raw(
        "agent.yield",
        json!({"kind":"budget","question":"step budget exhausted"}),
    );
    update(
        &mut s,
        ClientEvent::ChildEnvelope {
            generation: Generation(0),
            child_session_id: Ulid(2).to_string(),
            envelope: script.out[0].clone(),
        },
    )
    .unwrap();
    update(
        &mut s,
        ClientEvent::Resync {
            generation: Generation(0),
        },
    )
    .unwrap();
    spawn_subagent(&mut s, None);
    assert_eq!(s.children[0].status, ChildStatus::OutOfSteps);
    assert_eq!(s.children[0].child_seq, Some(10));
    let mut ui = UiState::default();
    handle_action(&mut s, &mut ui, Action::OpenChild(Ulid(2).to_string()));
    let generation = s.generation.clone();
    let mut metadata = s.tabs[0].session.clone().unwrap();
    metadata.session_id = Ulid(2).to_string();
    metadata.parent_session_id = Some(Ulid(1).to_string());
    metadata.promptable = false;
    update(
        &mut s,
        ClientEvent::Metadata {
            generation,
            result: Ok(metadata),
        },
    )
    .unwrap();
    handle_action(&mut s, &mut ui, Action::ContinueChild);
    assert!(ui.child_confirm.is_some());
}

#[test]
fn completion_keeps_the_child_terminal_after_its_parent_tab_closes() {
    let mut s = subagent_parent();
    spawn_subagent(&mut s, None);
    let mut ui = UiState::default();
    handle_action(&mut s, &mut ui, Action::OpenChild(Ulid(2).to_string()));
    s.active = 0;
    subagent_event(
        &mut s,
        2,
        "subagent.done",
        json!({"parent_session_id":Ulid(1),"child_session_id":Ulid(2),"agent":"vigie","outcome":"failed"}),
    );
    assert!(
        matches!(&s.conversation.blocks.last().unwrap().kind, BlockKind::Notice(text) if text.contains("alt+s open transcript"))
    );
    handle_action(&mut s, &mut ui, Action::CloseTab);
    assert!(s.read_only());
    assert_eq!(s.active_child_status(), ChildStatus::Failed);
}

#[test]
fn session_list_and_metadata_strip_terminal_controls_before_display() {
    let mut s = subagent_parent();
    let entry = serde_json::from_value(json!({"id":Ulid(2),"title":"\u{1b}[31mGoal\u{1b}[0m","created_at":"now","updated_at":"now","message_count":1,"live":true,"agent":"\u{1b}[31mvigie\u{1b}[0m","parent_session_id":Ulid(1),"child_status":"running","promptable":false})).unwrap();
    update(
        &mut s,
        ClientEvent::Sessions {
            generation: Generation(0),
            result: Ok(SessionListBody {
                sessions: vec![entry],
            }),
        },
    )
    .unwrap();
    assert_eq!(s.children[0].name, "vigie");
    assert_eq!(s.sessions[0].title.as_deref(), Some("Goal"));
    let mut metadata = s.session.clone().unwrap();
    metadata.agent = Some("\u{1b}[31mlux\u{1b}[0m".into());
    metadata.model = "\u{1b}[31mfast\u{1b}[0m".into();
    update(
        &mut s,
        ClientEvent::Metadata {
            generation: Generation(0),
            result: Ok(metadata),
        },
    )
    .unwrap();
    assert_eq!(s.agent_name(), "lux");
    assert_eq!(s.selected_model_id.as_deref(), Some("fast"));
}

#[test]
fn late_permission_mirrors_and_replayed_child_events_do_not_hide_continue() {
    let mut s = subagent_parent();
    spawn_subagent(&mut s, None);
    let mut script = Script::new();
    script.seq = 10;
    script.raw(
        "agent.yield",
        json!({"kind":"budget","question":"step budget exhausted"}),
    );
    update(
        &mut s,
        ClientEvent::ChildEnvelope {
            generation: Generation(0),
            child_session_id: Ulid(2).to_string(),
            envelope: script.out[0].clone(),
        },
    )
    .unwrap();
    subagent_event(
        &mut s,
        2,
        "permission.resolved",
        json!({"request_id":"old","resolution":"allow_once","child_session_id":Ulid(2)}),
    );
    assert_eq!(s.children[0].status, ChildStatus::OutOfSteps);
    script.seq = 5;
    script.raw(
        "permission.resolved",
        json!({"request_id":"old","resolution":"allow_once"}),
    );
    update(
        &mut s,
        ClientEvent::ChildEnvelope {
            generation: Generation(0),
            child_session_id: Ulid(2).to_string(),
            envelope: script.out[1].clone(),
        },
    )
    .unwrap();
    assert_eq!(s.children[0].status, ChildStatus::OutOfSteps);
}

#[test]
fn a_terminal_list_status_updates_a_child_without_an_open_parent() {
    let mut s = subagent_parent();
    s.session.as_mut().unwrap().session_id = Ulid(2).to_string();
    s.parent_session_id = Some(Ulid(1).to_string());
    s.child_status = ChildStatus::Running;
    let entry = serde_json::from_value(json!({"id":Ulid(2),"created_at":"now","updated_at":"now","message_count":1,"live":false,"parent_session_id":Ulid(1),"child_status":"done","promptable":false})).unwrap();
    update(
        &mut s,
        ClientEvent::Sessions {
            generation: Generation(0),
            result: Ok(SessionListBody {
                sessions: vec![entry],
            }),
        },
    )
    .unwrap();
    assert_eq!(s.active_child_status(), ChildStatus::Done);
}

#[tokio::test]
async fn child_watches_keep_the_parent_owner_are_idempotent_and_end_on_completion() {
    use withnav::tui::{
        client::Client,
        input::Editor,
        runtime::{Context, Executor},
    };
    let mut s = subagent_parent();
    s.tabs[0].id = 11;
    s.tabs.push(Tab {
        id: 22,
        generation: Generation(8),
        ..Default::default()
    });
    s.active = 1;
    let entry = serde_json::from_value(json!({"id":Ulid(2),"created_at":"now","updated_at":"now","message_count":0,"agent":"vigie","live":true,"parent_session_id":Ulid(1),"child_status":"running","promptable":false})).unwrap();
    let effects = update(
        &mut s,
        ClientEvent::Sessions {
            generation: Generation(0),
            result: Ok(SessionListBody {
                sessions: vec![entry],
            }),
        },
    )
    .unwrap();
    assert!(
        matches!(&effects[0], Effect::WatchChild { tab_id:11, generation:Generation(0), parent_session_id, .. } if parent_session_id == &Ulid(1).to_string())
    );
    let (tx, _) = tokio::sync::mpsc::channel(16);
    let mut exec = Executor::new(Client::new("http://127.0.0.1:1", None), tx);
    let ctx = Context {
        generation: Generation(8),
        session_id: None,
        workspace: "/ws".into(),
        config_dir: None,
    };
    let mut editor = Editor::new();
    exec.execute(effects[0].clone(), &ctx, &mut editor);
    exec.execute(effects[0].clone(), &ctx, &mut editor);
    assert_eq!(exec.stream_count(), 1);
    s.active = 0;
    let effects = subagent_event(
        &mut s,
        1,
        "subagent.done",
        json!({"parent_session_id":Ulid(1),"child_session_id":Ulid(2),"agent":"vigie","outcome":"done"}),
    );
    assert!(effects.contains(&Effect::UnwatchChild(Ulid(2).to_string())));
    for effect in effects {
        exec.execute(effect, &ctx, &mut editor);
    }
    assert_eq!(exec.stream_count(), 0);
}

#[test]
fn a_child_spawn_event_on_its_own_stream_adds_no_self_child() {
    let mut s = subagent_parent();
    s.session.as_mut().unwrap().session_id = Ulid(2).to_string();
    s.parent_session_id = Some(Ulid(1).to_string());
    let effects = subagent_event(
        &mut s,
        1,
        "subagent.spawned",
        json!({"parent_session_id":Ulid(1),"child_session_id":Ulid(2),"agent":"vigie","goal":"Read tests"}),
    );
    assert!(s.children.is_empty());
    assert!(effects.is_empty());
}

#[test]
fn startup_child_metadata_opens_read_only_without_resume() {
    let mut s = State::default();
    let effects = handle_action(
        &mut s,
        &mut UiState::default(),
        Action::ResumeSession("child".into()),
    );
    assert_eq!(effects, vec![Effect::FetchMetadata("child".into())]);
    let metadata = serde_json::from_value(json!({"session_id":"child","actor_state":"active","workspace":"/ws","model":"fast","agent":"vigie","current_seq":0,"parent_session_id":"parent","promptable":false,"child_status":"running"})).unwrap();
    let generation = s.generation.clone();
    let effects = update(
        &mut s,
        ClientEvent::Metadata {
            generation,
            result: Ok(metadata),
        },
    )
    .unwrap();
    assert!(s.read_only());
    assert!(effects.contains(&Effect::Subscribe {
        session_id: "child".into(),
        cursor: None
    }));
    assert!(!effects
        .iter()
        .any(|e| matches!(e, Effect::Resume(_) | Effect::Create(_))));
}

#[tokio::test]
async fn child_controls_and_permission_answers_use_the_owner_routes() {
    use wiremock::{
        matchers::{body_json, method, path},
        Mock, MockServer, ResponseTemplate,
    };
    use withnav::tui::client::Client;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions/child/stop"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"accepted":true,"actor_state":"stopped"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions/parent/children/child/continue"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error":"child_not_parked"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions/parent/permissions/request"))
        .and(body_json(
            json!({"decision":"deny","message":"Stay in the workspace"}),
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(server.uri(), None);
    assert!(client.stop("child").await.unwrap().accepted);
    assert!(client
        .continue_child("parent", "child")
        .await
        .unwrap_err()
        .contains("child_not_parked"));
    client
        .answer_permission(
            "parent",
            "request",
            PermissionAnswer {
                decision: "deny".into(),
                message: Some("Stay in the workspace".into()),
            },
        )
        .await
        .unwrap();
}

fn subagent_parent() -> State {
    let tab = Tab {
        session: Some(SessionMetadata {
            bypass: false,
            bypass_seq: 0,
            incarnation: None,
            source: "config".into(),
            variant_source: "model_default".into(),
            read_only: false,
            resume_disabled_reason: None,
            session_id: Ulid(1).to_string(),
            actor_state: "idle".into(),
            workspace: "/ws".into(),
            model: "fast".into(),
            agent: Some("build".into()),
            variant: None,
            parent_session_id: None,
            promptable: true,
            child_status: None,
            current_seq: 0,
        }),
        ..Default::default()
    };
    State {
        tabs: vec![tab],
        ..Default::default()
    }
}

fn subagent_event(s: &mut State, seq: u64, kind: &str, payload: Value) -> Vec<Effect> {
    update(
        s,
        ClientEvent::Envelope {
            generation: s.generation.clone(),
            envelope: EventEnvelope {
                schema_version: 1,
                event_id: Ulid(seq as u128 + 10),
                seq,
                session_id: Ulid(1),
                actor_id: ROOT_ACTOR_ID.into(),
                kind: kind.into(),
                occurred_at: "2026-10-04T00:00:00Z".into(),
                causation_id: None,
                correlation_id: None,
                payload,
            },
        },
    )
    .unwrap()
}

fn spawn_subagent(s: &mut State, delivery: Option<&str>) {
    let mut payload = json!({"parent_session_id": Ulid(1), "child_session_id": Ulid(2), "agent":"vigie", "goal":"Read the parser tests", "tool_call_id":"task-1"});
    if let Some(delivery) = delivery {
        payload["delivery"] = json!(delivery);
    }
    assert!(subagent_event(s, 1, "subagent.spawned", payload)
        .iter().any(|e| matches!(e, Effect::WatchChild { child_session_id, .. } if child_session_id == &Ulid(2).to_string())));
}

#[test]
fn subagents_replay_running_and_every_terminal_outcome() {
    for outcome in ["done", "failed", "stopped"] {
        let mut s = subagent_parent();
        spawn_subagent(&mut s, None);
        assert_eq!(s.children[0].status, ChildStatus::Running);
        let payload = json!({"parent_session_id": Ulid(1), "child_session_id": Ulid(2), "agent":"vigie", "outcome":outcome,"notification_id":Ulid(3), "reason":"server reported this"});
        subagent_event(&mut s, 2, "subagent.done", payload.clone());
        let children = s.children.clone();
        let blocks = s.conversation.blocks.clone();
        update(
            &mut s,
            ClientEvent::Resync {
                generation: Generation(0),
            },
        )
        .unwrap();
        assert_eq!(s.children, children);
        spawn_subagent(&mut s, None);
        subagent_event(&mut s, 2, "subagent.done", payload);
        assert_eq!(s.children, children);
        assert_eq!(s.conversation.blocks, blocks);
        assert_eq!(s.children[0].status.label(), outcome);
        assert_eq!(s.conversation.blocks.len(), 1);
    }
}

#[test]
fn optional_subagent_delivery_and_watchdog_fields_are_tolerated() {
    for delivery in [None, Some("next_turn"), Some("steer"), Some("interrupt")] {
        let mut s = subagent_parent();
        spawn_subagent(&mut s, delivery);
        assert_eq!(s.children[0].delivery.as_deref(), delivery);
        subagent_event(
            &mut s,
            2,
            "subagent.watchdog",
            json!({"child_session_id":Ulid(2),"verdict":"doubt","reason":"No recent progress","checked_at":"2026-10-04T00:00:00Z"}),
        );
        assert_eq!(s.children[0].status, ChildStatus::Doubt);
        subagent_event(
            &mut s,
            3,
            "subagent.watchdog",
            json!({"child_session_id":Ulid(2),"verdict":"active"}),
        );
        assert_eq!(s.children[0].status, ChildStatus::Running);
        subagent_event(
            &mut s,
            4,
            "subagent.done",
            json!({"parent_session_id":Ulid(1),"child_session_id":Ulid(2),"agent":"vigie","outcome":"done","delivery":delivery}),
        );
        subagent_event(
            &mut s,
            5,
            "subagent.watchdog",
            json!({"child_session_id":Ulid(2),"verdict":"doubt"}),
        );
        assert_eq!(s.children[0].status, ChildStatus::Done);
    }
}

#[test]
fn opening_and_closing_a_child_never_resumes_or_writes_to_it() {
    let mut s = subagent_parent();
    spawn_subagent(&mut s, None);
    let mut ui = UiState::default();
    let id = Ulid(2).to_string();
    let effects = handle_action(&mut s, &mut ui, Action::OpenChild(id.clone()));
    assert!(effects.contains(&Effect::FetchMetadata(id.clone())));
    assert!(effects.contains(&Effect::Subscribe {
        session_id: id.clone(),
        cursor: None
    }));
    assert!(!effects
        .iter()
        .any(|e| matches!(e, Effect::Resume(_) | Effect::Create(_))));
    assert!(s.read_only());
    assert!(handle_action(&mut s, &mut ui, Action::Submit("hello".into())).is_empty());
    assert!(handle_action(
        &mut s,
        &mut ui,
        Action::RunCommand {
            name: "compact".into(),
            arguments: String::new(),
            revision: None
        }
    )
    .is_empty());
    handle_action(&mut s, &mut ui, Action::CloseTab);
    assert_eq!(s.active, 0);
    assert_eq!(s.session.as_ref().unwrap().session_id, Ulid(1).to_string());
    assert!(!s.read_only());
}

#[test]
fn subagent_controls_confirm_effects_and_show_server_refusals() {
    for (status, action, control) in [
        (ChildStatus::Running, Action::Stop, ChildControl::Stop),
        (
            ChildStatus::OutOfSteps,
            Action::ContinueChild,
            ChildControl::Continue,
        ),
    ] {
        let mut s = subagent_parent();
        spawn_subagent(&mut s, None);
        s.children[0].status = status;
        let mut ui = UiState::default();
        handle_action(&mut s, &mut ui, Action::OpenChild(Ulid(2).to_string()));
        let generation = s.generation.clone();
        update(
            &mut s,
            ClientEvent::Metadata {
                generation: generation.clone(),
                result: Ok(SessionMetadata {
                    bypass: false,
                    bypass_seq: 0,
                    incarnation: None,
                    source: "config".into(),
                    variant_source: "model_default".into(),
                    read_only: false,
                    resume_disabled_reason: None,
                    session_id: Ulid(2).to_string(),
                    actor_state: "active".into(),
                    workspace: "/ws".into(),
                    model: "fast".into(),
                    agent: Some("vigie".into()),
                    variant: None,
                    parent_session_id: None,
                    promptable: true,
                    child_status: None,
                    current_seq: 0,
                }),
            },
        )
        .unwrap();
        assert!(handle_action(&mut s, &mut ui, action).is_empty());
        assert!(ui.child_confirm.is_some());
        assert_eq!(
            handle_action(&mut s, &mut ui, Action::ConfirmChildControl),
            vec![Effect::ControlChild {
                pause_token: None,
                parent_session_id: Ulid(1).to_string(),
                child_session_id: Ulid(2).to_string(),
                control
            }]
        );
        update(
            &mut s,
            ClientEvent::ChildControlled {
                pause_token: None,
                generation,
                child_session_id: Ulid(2).to_string(),
                control,
                result: Err("HTTP 409 Conflict: child_not_parked".into()),
            },
        )
        .unwrap();
        assert!(
            matches!(&s.conversation.blocks.last().unwrap().kind, BlockKind::Error(text) if text.contains("child_not_parked"))
        );
        assert_eq!(s.active_child_status(), status);
    }
}

#[test]
fn child_permission_is_labelled_and_answered_on_the_parent() {
    let mut s = subagent_parent();
    spawn_subagent(&mut s, None);
    subagent_event(
        &mut s,
        2,
        "permission.requested",
        json!({"request_id":"request-1","tool_call_id":"child-call","tool":"bash","arguments":"{\"command\":\"cargo test\"}","scope":{"kind":"folder","value":"/ws"},"agent":"vigie","reason":"needs approval","child_session_id":Ulid(2)}),
    );
    assert_eq!(s.children[0].status, ChildStatus::Permission);
    assert!(s
        .open_permission()
        .unwrap()
        .reason
        .contains("subagent vigie"));
    let answer = PermissionAnswer {
        decision: "allow_once".into(),
        message: None,
    };
    assert_eq!(
        handle_action(
            &mut s,
            &mut UiState::default(),
            Action::AnswerPermission {
                request_id: "request-1".into(),
                answer: answer.clone()
            }
        ),
        vec![Effect::AnswerPermission {
            session_id: Ulid(1).to_string(),
            request_id: "request-1".into(),
            answer
        }]
    );
    subagent_event(
        &mut s,
        3,
        "permission.resolved",
        json!({"request_id":"request-1","tool_call_id":"child-call","resolution":"allow_once","child_session_id":Ulid(2)}),
    );
    assert_eq!(s.children[0].status, ChildStatus::Running);
    assert!(s.open_permission().is_none());
}

#[test]
fn child_budget_events_update_the_sidebar_without_opening_a_transcript() {
    let mut s = subagent_parent();
    spawn_subagent(&mut s, None);
    let mut script = Script::new();
    script.raw(
        "agent.yield",
        json!({"kind":"budget","question":"step budget exhausted"}),
    );
    update(
        &mut s,
        ClientEvent::ChildEnvelope {
            generation: Generation(0),
            child_session_id: Ulid(2).to_string(),
            envelope: script.out[0].clone(),
        },
    )
    .unwrap();
    assert_eq!(s.children[0].status, ChildStatus::OutOfSteps);
    assert!(s.conversation.blocks.is_empty());
}

#[test]
fn session_list_seeds_real_children_without_inventing_goals() {
    let mut s = subagent_parent();
    let entry: SessionListEntry = serde_json::from_value(json!({"id":Ulid(2),"title":"Not a delegated goal","created_at":"now","updated_at":"now","message_count":1,"agent":"lux","live":false,"parent_session_id":Ulid(1),"child_status":"stopped","promptable":false})).unwrap();
    update(
        &mut s,
        ClientEvent::Sessions {
            generation: Generation(0),
            result: Ok(SessionListBody {
                sessions: vec![entry],
            }),
        },
    )
    .unwrap();
    assert_eq!(s.children[0].name, "lux");
    assert_eq!(s.children[0].status, ChildStatus::Stopped);
    assert!(s.children[0].goal.is_empty());
}

// ---------------------------------------------------------------- fixture generator

struct Script {
    seq: u64,
    out: Vec<EventEnvelope>,
}
impl Script {
    fn new() -> Self {
        Self {
            seq: 0,
            out: vec![],
        }
    }
    fn raw(&mut self, kind: &str, payload: Value) -> &mut Self {
        self.out.push(EventEnvelope {
            schema_version: 1,
            event_id: Ulid(1000 + self.seq as u128),
            seq: self.seq,
            session_id: Ulid(1),
            actor_id: ROOT_ACTOR_ID.into(),
            kind: kind.into(),
            occurred_at: "2026-10-02T00:00:00.000Z".into(),
            causation_id: None,
            correlation_id: None,
            payload,
        });
        self.seq += 1;
        self
    }
    fn push<T: Serialize>(&mut self, kind: EventKind, p: &T) -> &mut Self {
        self.raw(kind.as_str(), serde_json::to_value(p).unwrap())
    }
    fn start(&mut self) -> &mut Self {
        self.push(
            EventKind::SessionStart,
            &SessionStart {
                workspace: "/ws".into(),
                root_actor_id: ROOT_ACTOR_ID.into(),
            },
        )
    }
    fn user(&mut self, text: &str) -> &mut Self {
        self.push(
            EventKind::MessageReceived,
            &MessageReceived {
                selected_skill: None,
                command_id: Ulid(7),
                source: "user:http".into(),
                target: ROOT_ACTOR_ID.into(),
                mode: "next-turn".into(),
                content: text.into(),
            },
        )
    }
    fn turn_start(&mut self, turn: u128) -> &mut Self {
        self.push(
            EventKind::TurnStart,
            &TurnStart {
                turn_id: Ulid(turn),
                actor_id: ROOT_ACTOR_ID.into(),
                task_id: Ulid(9),
            },
        )
    }
    fn turn_end(&mut self, turn: u128, outcome: TurnEndOutcome, usage: Option<Usage>) -> &mut Self {
        self.push(
            EventKind::TurnEnd,
            &TurnEnd {
                turn_id: Ulid(turn),
                actor_id: ROOT_ACTOR_ID.into(),
                outcome,
                usage,
                duration_ms: 5,
                error: None,
            },
        )
    }
    fn delta(&mut self, turn: u128, step: u128, delta: StreamDelta) -> &mut Self {
        self.push(
            EventKind::LlmDelta,
            &LlmDelta {
                turn_id: Ulid(turn),
                step_id: Ulid(step),
                delta,
            },
        )
    }
    fn response(
        &mut self,
        turn: u128,
        step: u128,
        text: &str,
        thinking: &str,
        calls: Vec<ToolCall>,
        usage: Option<Usage>,
    ) -> &mut Self {
        self.push(
            EventKind::LlmResponse,
            &LlmResponse {
                turn_id: Ulid(turn),
                step_id: Ulid(step),
                message: AssistantMessage {
                    text: text.into(),
                    thinking: thinking.into(),
                    tool_calls: calls,
                    finish_reason: None,
                    usage,
                },
                usage,
                finish_reason: None,
            },
        )
    }
    fn pre(&mut self, id: &str, name: &str, args: Value) -> &mut Self {
        self.push(
            EventKind::ToolPreCall,
            &ToolPreCall {
                tool_call_id: id.into(),
                name: name.into(),
                source: ToolSource::Native,
                args,
                actor_id: ROOT_ACTOR_ID.into(),
            },
        )
    }
    fn post(
        &mut self,
        id: &str,
        name: &str,
        status: ToolStatus,
        out: &str,
        meta: Value,
    ) -> &mut Self {
        self.push(
            EventKind::ToolPostCall,
            &ToolPostCall {
                tool_call_id: id.into(),
                name: name.into(),
                status,
                output: out.into(),
                metadata: meta,
            },
        )
    }
}

fn usage(p: u64, c: u64) -> Usage {
    Usage {
        prompt_tokens: p,
        completion_tokens: c,
        total_tokens: p + c,
    }
}
fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        args,
        args_valid: true,
    }
}
fn text(t: &str) -> StreamDelta {
    StreamDelta::Text { text: t.into() }
}
fn thinking(t: &str) -> StreamDelta {
    StreamDelta::Thinking { text: t.into() }
}
fn without_deltas(v: &[EventEnvelope]) -> Vec<EventEnvelope> {
    v.iter()
        .filter(|e| e.kind != "llm.delta")
        .cloned()
        .collect()
}

fn turn_live() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("hello").turn_start(100);
    s.delta(100, 201, thinking("Let me "))
        .delta(100, 201, thinking("think."));
    s.delta(100, 201, text("Hi "))
        .delta(100, 201, text("there"));
    s.response(
        100,
        201,
        "Hi there",
        "Let me think.",
        vec![],
        Some(usage(10, 5)),
    );
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(10, 5)));
    s.out
}

fn tools_live() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("fix it").turn_start(100);
    s.delta(100, 201, thinking("plan"));
    s.delta(
        100,
        201,
        StreamDelta::ToolCallStarted {
            index: 0,
            name: "read".into(),
        },
    );
    s.delta(
        100,
        201,
        StreamDelta::ToolCallArgs {
            index: 0,
            fragment: "{\"path\":\"a".into(),
        },
    );
    s.delta(
        100,
        201,
        StreamDelta::ToolCallArgs {
            index: 0,
            fragment: ".rs\"}".into(),
        },
    );
    s.delta(
        100,
        201,
        StreamDelta::ToolCallStarted {
            index: 1,
            name: "bash".into(),
        },
    );
    s.delta(
        100,
        201,
        StreamDelta::ToolCallArgs {
            index: 1,
            fragment: "{\"command\":\"ls\"}".into(),
        },
    );
    s.response(
        100,
        201,
        "Reading.",
        "plan",
        vec![
            call("c1", "read", json!({"path":"a.rs"})),
            call("c2", "bash", json!({"command":"ls"})),
        ],
        Some(usage(100, 20)),
    );
    s.pre("c1", "read", json!({"path":"a.rs"}));
    s.post(
        "c1",
        "read",
        ToolStatus::Ok,
        "fn main(){}",
        json!({"duration_ms":18}),
    );
    s.pre("c2", "bash", json!({"command":"ls"}));
    s.post(
        "c2",
        "bash",
        ToolStatus::Error,
        "boom",
        json!({"exit_code":101,"duration_ms":1420}),
    );
    s.delta(100, 202, text("Fixed")).delta(100, 202, text("!"));
    s.response(100, 202, "Fixed!", "", vec![], Some(usage(150, 10)));
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(150, 10)));
    s.out
}

fn tool_statuses() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("go").turn_start(100);
    s.response(
        100,
        201,
        "",
        "",
        vec![
            call("ok", "read", json!({"path":"x"})),
            call("err", "bash", json!({"command":"false"})),
            call("den", "write", json!({"path":"/etc/x"})),
            call("int", "bash", json!({"command":"sleep 9"})),
        ],
        Some(usage(10, 1)),
    );
    for (id, name, args) in [
        ("ok", "read", json!({"path":"x"})),
        ("err", "bash", json!({"command":"false"})),
        ("den", "write", json!({"path":"/etc/x"})),
        ("int", "bash", json!({"command":"sleep 9"})),
    ] {
        s.pre(id, name, args);
    }
    s.post(
        "ok",
        "read",
        ToolStatus::Ok,
        "data",
        json!({"duration_ms":3}),
    );
    s.post(
        "err",
        "bash",
        ToolStatus::Error,
        "\u{1b}[31mfail\u{1b}[0m",
        json!({"exit_code":1,"duration_ms":2000}),
    );
    s.post("den", "write", ToolStatus::Denied, "denied", Value::Null);
    s.post("int", "bash", ToolStatus::Interrupted, "", json!({}));
    // post-call whose pre-call and response were never seen
    s.post(
        "orphan",
        "grep",
        ToolStatus::Ok,
        "match",
        json!({"duration_ms":1}),
    );
    s.response(100, 202, "after", "", vec![], Some(usage(20, 2)));
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(20, 2)));
    s.out
}

fn interrupted_turn() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("go").turn_start(100);
    s.response(
        100,
        201,
        "",
        "",
        vec![
            call("t1", "bash", json!({"command":"sleep 9"})),
            call("t2", "read", json!({"path":"x"})),
            call("t3", "write", json!({"path":"y"})),
        ],
        Some(usage(10, 1)),
    );
    s.pre("t1", "bash", json!({"command":"sleep 9"}));
    s.turn_end(100, TurnEndOutcome::Interrupted, None);
    s.out
}

fn yield_ask() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("flash it").turn_start(100);
    s.response(
        100,
        201,
        "",
        "",
        vec![call("a1", "ask", json!({"question":"insert SD card"}))],
        Some(usage(10, 1)),
    );
    s.push(
        EventKind::AgentYield,
        &AgentYield {
            actor_id: ROOT_ACTOR_ID.into(),
            turn_id: Ulid(100),
            question: "insert SD card".into(),
            kind: Some("ask".into()),
            details: None,
        },
    );
    s.turn_end(100, TurnEndOutcome::Yielded, Some(usage(10, 1)));
    s.raw(
        "agent.state",
        json!({"actor_id":"agent:main","from":"running","to":"yielded"}),
    );
    s.user("done");
    s.turn_start(101);
    s.response(101, 202, "thanks", "", vec![], Some(usage(30, 2)));
    s.turn_end(101, TurnEndOutcome::Done, Some(usage(30, 2)));
    s.out
}

fn usage_fallback() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("go").turn_start(100);
    s.response(
        100,
        201,
        "a",
        "",
        vec![call("c1", "read", json!({}))],
        Some(usage(100, 10)),
    );
    s.pre("c1", "read", json!({}));
    s.post("c1", "read", ToolStatus::Ok, "x", json!({}));
    s.response(100, 202, "b", "", vec![], None);
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(150, 20)));
    s.out
}

fn usage_missing() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("go").turn_start(100);
    s.response(100, 201, "a", "", vec![], None);
    s.turn_end(100, TurnEndOutcome::Done, None);
    s.out
}

fn unknown_kind() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("hi");
    s.raw("future.kind", json!({"anything": [1, 2]}));
    s.raw("task.done", json!({}));
    s.user("again");
    s.out
}

fn stopped() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("hi").turn_start(100);
    s.response(100, 201, "hello", "", vec![], Some(usage(10, 1)));
    s.turn_end(100, TurnEndOutcome::Done, Some(usage(10, 1)));
    s.push(
        EventKind::SessionEnd,
        &SessionEnd {
            reason: "stop".into(),
            final_seq: 5,
        },
    );
    s.out
}

fn resumed() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.seq = 6;
    s.start().user("again").turn_start(102);
    s.response(102, 301, "welcome back", "", vec![], Some(usage(20, 2)));
    s.turn_end(102, TurnEndOutcome::Done, Some(usage(20, 2)));
    s.out
}

const QUOTA_MESSAGE: &str = "provider API error (status 429): Quota preflight blocked";

fn error_live() -> Vec<EventEnvelope> {
    let mut s = Script::new();
    s.start().user("hello").turn_start(100);
    s.push(
        EventKind::TurnEnd,
        &TurnEnd {
            turn_id: Ulid(100),
            actor_id: ROOT_ACTOR_ID.into(),
            outcome: TurnEndOutcome::Error,
            usage: None,
            duration_ms: 5,
            error: Some(TurnError {
                code: "provider".into(),
                message: format!("{QUOTA_MESSAGE}\n{}", "x".repeat(400)),
            }),
        },
    );
    s.out
}

fn scenarios() -> Vec<(&'static str, Vec<EventEnvelope>)> {
    let (t, l) = (turn_live(), tools_live());
    vec![
        ("turn_replay", without_deltas(&t)),
        ("turn_live", t),
        ("tools_replay", without_deltas(&l)),
        ("tools_live", l),
        ("error_replay", without_deltas(&error_live())),
        ("error_live", error_live()),
        ("tool_statuses", tool_statuses()),
        ("interrupted_turn", interrupted_turn()),
        ("yield_ask", yield_ask()),
        ("usage_fallback", usage_fallback()),
        ("usage_missing", usage_missing()),
        ("unknown_kind", unknown_kind()),
        ("stopped", stopped()),
        ("resumed", resumed()),
    ]
}
fn jsonl(v: &[EventEnvelope]) -> String {
    v.iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect()
}

#[test]
#[ignore = "rewrites tests/fixtures/tui/*.jsonl"]
fn regenerate_fixtures() {
    std::fs::create_dir_all(DIR).unwrap();
    for (name, envs) in scenarios() {
        std::fs::write(format!("{DIR}/{name}.jsonl"), jsonl(&envs)).unwrap();
    }
}

#[test]
fn committed_fixtures_match_the_generator() {
    for (name, envs) in scenarios() {
        let on_disk = std::fs::read_to_string(format!("{DIR}/{name}.jsonl")).unwrap();
        assert_eq!(on_disk, jsonl(&envs), "{name}.jsonl is stale; regenerate");
    }
}

// ---------------------------------------------------------------- helpers

fn load(name: &str) -> Vec<EventEnvelope> {
    std::fs::read_to_string(format!("{DIR}/{name}.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn st() -> State {
    State {
        workspace: "/ws".into(),
        ..Default::default()
    }
}
fn envelope(s: &State, e: &EventEnvelope) -> ClientEvent {
    ClientEvent::Envelope {
        generation: s.generation.clone(),
        envelope: e.clone(),
    }
}
fn feed(s: &mut State, envs: &[EventEnvelope]) {
    for e in envs {
        let ev = envelope(s, e);
        update(s, ev).unwrap();
    }
}
fn run(name: &str) -> State {
    let mut s = st();
    feed(&mut s, &load(name));
    s
}
fn ui() -> UiState {
    UiState::default()
}
fn ids(s: &State) -> Vec<String> {
    s.conversation.blocks.iter().map(|b| b.id.clone()).collect()
}
#[derive(Debug, Clone)]
struct Tool {
    name: String,
    args: Value,
    status: Option<ToolStatus>,
    output: String,
    duration_ms: Option<u64>,
    exit_code: Option<i32>,
}
fn tool(s: &State, id: &str) -> Tool {
    let bid = format!("tool:{id}");
    match s
        .conversation
        .blocks
        .iter()
        .find(|b| b.id == bid)
        .map(|b| &b.kind)
    {
        Some(BlockKind::ToolCall {
            name,
            args,
            status,
            output,
            duration_ms,
            exit_code,
            ..
        }) => Tool {
            name: name.clone(),
            args: args.clone(),
            status: *status,
            output: output.clone(),
            duration_ms: *duration_ms,
            exit_code: *exit_code,
        },
        other => panic!("no tool block {bid}: {other:?}"),
    }
}
fn steps(s: &State) -> Vec<(String, String)> {
    s.conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::AssistantStep { text, thinking, .. } => {
                Some((text.clone(), thinking.clone()))
            }
            _ => None,
        })
        .collect()
}
fn with_session(s: &mut State, id: &str) {
    s.session = Some(SessionMetadata {
        bypass: false,
        bypass_seq: 0,
        incarnation: None,
        source: "config".into(),
        variant_source: "model_default".into(),
        read_only: false,
        resume_disabled_reason: None,
        session_id: id.into(),
        actor_state: "idle".into(),
        workspace: "/ws".into(),
        model: "a".into(),
        agent: None,
        variant: None,
        parent_session_id: None,
        promptable: true,
        child_status: None,
        current_seq: 0,
    });
}
fn created(s: &State, id: &str) -> ClientEvent {
    ClientEvent::Created {
        generation: s.generation.clone(),
        result: Ok(CreateSessionResponse {
            session_id: id.into(),
            root_actor_id: "agent:main".into(),
            actor_state: "idle".into(),
        }),
    }
}
fn resumed_ev(s: &State, id: &str) -> ClientEvent {
    ClientEvent::Resumed {
        generation: s.generation.clone(),
        result: Ok(CreateSessionResponse {
            session_id: id.into(),
            root_actor_id: "agent:main".into(),
            actor_state: "idle".into(),
        }),
    }
}
fn catalog() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            api: withnav::tui::protocol::ApiKind::Chat,
            gateway: "gateway".into(),
            provider_group: "gateway".into(),
            name: "a".into(),
            upstream_id: "vendor/a".into(),
            context_window: Some(1000),
            ..Default::default()
        },
        ModelEntry {
            api: withnav::tui::protocol::ApiKind::Chat,
            gateway: "gateway".into(),
            provider_group: "gateway".into(),
            name: "b".into(),
            upstream_id: "vendor/b".into(),
            context_window: None,
            ..Default::default()
        },
    ]
}
fn resumed_now(s: &mut State, id: &str) -> Result<Vec<Effect>, ProtocolError> {
    let ev = resumed_ev(s, id);
    update(s, ev)
}
fn created_now(s: &mut State, id: &str) -> Result<Vec<Effect>, ProtocolError> {
    let ev = created(s, id);
    update(s, ev)
}
fn models_now(s: &mut State, default: Option<String>) -> Result<Vec<Effect>, ProtocolError> {
    update(
        s,
        ClientEvent::Models {
            workspace: "/ws".into(),
            request: 0,
            generation: s.generation.clone(),
            result: Ok(ModelsBody {
                models: catalog(),
                default_agent: Some("build".into()),
            }),
        },
    )?;
    if !withnav::tui::reducer::occupied(s) {
        let chosen = default.unwrap_or_else(|| "a".into());
        update(
            s,
            ClientEvent::Agents {
                workspace: s.workspace.clone(),
                request: 0,
                generation: s.generation.clone(),
                session_id: None,
                result: Ok(AgentsBody {
                    default_agent: "build".into(),
                    agents: vec![AgentEntry {
                        id: "build".into(),
                        mode: "primary".into(),
                        effective: Some(Selection {
                            model: chosen,
                            source: "config".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                }),
            },
        )?;
    }
    Ok(vec![])
}

fn submitted_now(
    s: &mut State,
    revision: u64,
    result: Result<SubmitResponse, String>,
) -> Result<Vec<Effect>, ProtocolError> {
    let ev = ClientEvent::Submitted {
        generation: s.generation.clone(),
        revision,
        result,
    };
    update(s, ev)
}
fn mutate(envs: &mut [EventEnvelope], f: impl Fn(&mut EventEnvelope)) {
    envs.iter_mut().for_each(f);
}

// ---------------------------------------------------------------- plan section 6

#[test]
fn delta_then_response_does_not_duplicate() {
    let live = load("turn_live");
    let mut s = st();
    // up to and including the deltas: the draft grows
    feed(&mut s, &live[..7]);
    assert_eq!(steps(&s), vec![("Hi there".into(), "Let me think.".into())]);
    feed(&mut s, &live[7..]);
    assert_eq!(steps(&s), vec![("Hi there".into(), "Let me think.".into())]);
    assert_eq!(
        ids(&s),
        vec!["1".to_string(), format!("step:{}", Ulid(201))]
    );
}

#[test]
fn several_steps_and_tools_in_a_turn() {
    let s = run("tools_replay");
    assert_eq!(
        steps(&s),
        vec![
            ("Reading.".into(), "plan".into()),
            ("Fixed!".into(), "".into())
        ]
    );
    assert_eq!(tool(&s, "c1").status, Some(ToolStatus::Ok));
    assert_eq!(tool(&s, "c2").status, Some(ToolStatus::Error));
    let key = ("agent:main".to_string(), Ulid(100).to_string());
    assert_eq!(s.conversation.tools_by_turn[&key], vec!["c1", "c2"]);
    assert_eq!(s.conversation.blocks.len(), 5); // user, step1, c1, c2, step2
    assert_eq!(s.activity, Activity::Idle);
}

#[test]
fn replay_equals_live() {
    for (live, replay) in [("turn_live", "turn_replay"), ("tools_live", "tools_replay")] {
        let (l, r) = (run(live), run(replay));
        assert!(!l.conversation.blocks.is_empty());
        assert_eq!(
            l.conversation.blocks, r.conversation.blocks,
            "{live} vs {replay}"
        );
        assert_eq!(l.conversation.session_tokens, r.conversation.session_tokens);
        assert_eq!(l.activity, r.activity);
    }
}

#[test]
fn disconnect_mid_step_then_canonical_recovery() {
    let (live, replay) = (load("tools_live"), load("tools_replay"));
    let mut s = st();
    feed(&mut s, &live[..8]); // streaming, no llm.response yet
    assert_eq!(steps(&s), vec![("".into(), "plan".into())]);
    for ev in [
        ClientEvent::StreamDisconnected {
            generation: s.generation.clone(),
        },
        ClientEvent::Reconnecting {
            generation: s.generation.clone(),
        },
        ClientEvent::StreamConnected {
            generation: s.generation.clone(),
        },
    ] {
        update(&mut s, ev).unwrap();
    }
    assert_eq!(s.connection, ConnectionStatus::Connected);
    let cursor = s.last_applied_seq.unwrap();
    let rest: Vec<_> = replay.iter().filter(|e| e.seq > cursor).cloned().collect();
    feed(&mut s, &rest);
    assert_eq!(
        s.conversation.blocks,
        run("tools_replay").conversation.blocks
    );
}

#[test]
fn duplicate_seqs_cursor_zero_and_gaps() {
    let mut s = st();
    assert_eq!(s.last_applied_seq, None);
    let replay = load("tools_replay"); // has gaps: llm.delta seqs are absent
    feed(&mut s, &replay[..1]);
    assert_eq!(s.last_applied_seq, Some(0)); // seq 0 is a real cursor
    feed(&mut s, &replay[..1]); // duplicate
    feed(&mut s, &replay);
    feed(&mut s, &replay); // whole replay again
    let once = run("tools_replay");
    assert_eq!(s.conversation.blocks, once.conversation.blocks);
    assert_eq!(
        s.conversation.session_tokens,
        once.conversation.session_tokens
    );
    assert_eq!(s.last_applied_seq, Some(replay.last().unwrap().seq));
}

#[test]
fn resync_resets_projection_and_stale_generation_is_dropped() {
    let mut s = run("tools_replay");
    let before = s.conversation.blocks.clone();
    let gen = s.generation.clone();
    let effects = update(
        &mut s,
        ClientEvent::Resync {
            generation: gen.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        effects,
        vec![Effect::Subscribe {
            session_id: String::new(),
            cursor: None
        }]
    );
    assert!(s.conversation.blocks.is_empty());
    assert_eq!(s.last_applied_seq, None);
    feed(&mut s, &load("tools_replay"));
    assert_eq!(s.conversation.blocks, before);
    // an envelope of an obsolete generation changes nothing
    let stale = ClientEvent::Envelope {
        generation: Generation(99),
        envelope: load("resumed")[1].clone(),
    };
    assert!(update(&mut s, stale).unwrap().is_empty());
    assert_eq!(s.conversation.blocks, before);
}

#[test]
fn tool_ok_error_denied_interrupted_missing_pre_call_exit_code_duration() {
    let s = run("tool_statuses");
    let ok = tool(&s, "ok");
    assert_eq!(
        (ok.status, ok.duration_ms, ok.exit_code, ok.output.as_str()),
        (Some(ToolStatus::Ok), Some(3), None, "data")
    );
    let err = tool(&s, "err");
    assert_eq!(
        (err.status, err.duration_ms, err.exit_code),
        (Some(ToolStatus::Error), Some(2000), Some(1))
    );
    assert_eq!(err.output, "fail"); // escape sequences stripped
    assert_eq!(tool(&s, "den").status, Some(ToolStatus::Denied));
    assert_eq!(tool(&s, "den").duration_ms, None);
    assert_eq!(tool(&s, "int").status, Some(ToolStatus::Interrupted));
    let orphan = tool(&s, "orphan");
    assert_eq!(
        (orphan.status, orphan.args, orphan.name.as_str()),
        (Some(ToolStatus::Ok), Value::Null, "grep")
    );
}

#[test]
fn yield_and_answer_including_ask() {
    let live = load("yield_ask");
    let mut s = st();
    feed(&mut s, &live[..6]); // through turn.end yielded
    assert_eq!(s.activity, Activity::AwaitingUser);
    assert!(s
        .conversation
        .blocks
        .iter()
        .any(|b| b.kind == BlockKind::YieldQuestion("insert SD card".into())));
    // the ask call had no pre/post pair: it is not left "running"
    assert_eq!(tool(&s, "a1").status, Some(ToolStatus::Ok));
    feed(&mut s, &live[6..]);
    assert_eq!(s.activity, Activity::Idle);
    assert!(s
        .conversation
        .blocks
        .iter()
        .any(|b| matches!(&b.kind, BlockKind::Message { text, .. } if text == "done")));
    assert_eq!(steps(&s).last().unwrap().0, "thanks");
}

#[test]
fn usage_once_per_step_and_final_usage_not_doubled() {
    let s = run("turn_replay");
    assert_eq!(s.conversation.session_tokens, Some(15)); // turn.end repeats the response usage
    assert_eq!(s.conversation.context_tokens, Some(10));
    assert_eq!(s.conversation.completion_tokens, Some(5));
    let s = run("tools_replay");
    assert_eq!(s.conversation.session_tokens, Some(120 + 160));
    assert_eq!(s.conversation.context_tokens, Some(150));
    assert!(!s.conversation.missing_usage);
}

#[test]
fn missing_usage_and_context_window_stay_unknown() {
    let s = run("usage_missing");
    assert_eq!(s.conversation.session_tokens, None);
    assert_eq!(s.conversation.context_tokens, None);
    assert!(s.conversation.missing_usage);
    let mut s = st();
    models_now(&mut s, Some("b".into())).unwrap();
    assert_eq!(s.selected_model_id.as_deref(), Some("b"));
    assert_eq!(s.context_window, None);
}

#[test]
fn unknown_kinds_are_ignored_but_advance_the_cursor() {
    let s = run("unknown_kind");
    assert_eq!(s.last_applied_seq, Some(4));
    assert_eq!(s.conversation.blocks.len(), 2);
    assert!(s.last_protocol_error.is_none());
}

#[test]
fn malformed_payload_and_schema_mismatch_are_protocol_errors() {
    let mut envs = load("turn_replay");
    let mut s = st();
    let user = envs.remove(1);
    let mut bad = user.clone();
    bad.payload = json!({"source": 5});
    let ev = envelope(&s, &bad);
    assert!(update(&mut s, ev).is_err());
    let mut bad = envs[1].clone(); // turn.start
    bad.kind = "llm.response".into();
    bad.payload = json!({"nope": 1});
    let ev = envelope(&s, &bad);
    assert!(update(&mut s, ev).is_err());
    let mut bad = user;
    bad.schema_version = 2;
    let ev = envelope(&s, &bad);
    assert_eq!(update(&mut s, ev).unwrap_err().0, "unsupported schema 2");
}

#[test]
fn stop_then_resume() {
    let mut s = run("stopped");
    with_session(&mut s, "S1");
    assert_eq!(s.activity, Activity::Stopped);
    // submitting after Stop resumes first, then subscribes from the cursor, then sends
    let fx = handle_action(
        &mut s,
        &mut ui(),
        Action::SubmitAt {
            text: "again".into(),
            revision: 4,
        },
    );
    assert_eq!(fx, vec![Effect::Resume("S1".into())]);
    let fx = resumed_now(&mut s, "S1").unwrap();
    assert_eq!(
        fx[2],
        Effect::Subscribe {
            session_id: "S1".into(),
            cursor: Some(5)
        }
    );
    assert!(matches!(&fx[3], Effect::SendAt { revision: 4, .. }));
    feed(&mut s, &load("resumed"));
    assert_eq!(s.activity, Activity::Idle);
    assert_eq!(steps(&s).len(), 2);
    assert_eq!(steps(&s)[1].0, "welcome back");
    assert_eq!(
        s.conversation
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Message { .. }))
            .count(),
        2
    );
}

// ---------------------------------------------------------------- regressions (bug numbers of the review)

#[test]
fn new_conversation_opens_a_tab_with_its_own_cursor_and_generation() {
    let mut s = run("turn_replay");
    with_session(&mut s, "S1");
    let old = s.generation.clone();
    let fx = handle_action(&mut s, &mut ui(), Action::NewConversation);
    assert_eq!(
        fx,
        vec![
            Effect::SetEditor {
                text: String::new(),
                revision: 0
            },
            Effect::FetchAgents(None)
        ]
    );
    assert_eq!((s.tabs.len(), s.active), (2, 1));
    assert_eq!(
        (s.last_applied_seq, s.activity, s.session.is_none()),
        (None, Activity::Idle, true)
    );
    assert!(s.conversation.blocks.is_empty() && s.conversation.pending_tools.is_empty());
    assert_ne!(s.generation, old);
    // the first tab keeps its conversation
    assert_eq!(s.tabs[0].last_applied_seq, Some(8));
    // the new conversation's seq 0 is applied to the new tab
    feed(&mut s, &load("turn_replay"));
    assert_eq!(s.last_applied_seq, Some(8));
    assert!(!s.conversation.blocks.is_empty());
    // a blank tab is reused rather than doubled
    let mut blank = st();
    handle_action(&mut blank, &mut ui(), Action::NewConversation);
    assert_eq!(blank.tabs.len(), 1);
}

#[test]
fn pick_model_with_a_session_opens_a_tab_without_one_it_does_not() {
    let mut s = run("turn_replay");
    models_now(&mut s, None).unwrap();
    let gen = s.generation.clone();
    let fx = handle_action(&mut s, &mut ui(), Action::PickModel("b".into()));
    assert!(fx.is_empty());
    assert_eq!(
        (s.generation.clone(), s.conversation.blocks.is_empty()),
        (gen.clone(), false)
    ); // no session: untouched
    with_session(&mut s, "S1");
    handle_action(&mut s, &mut ui(), Action::PickModel("a".into()));
    assert_eq!((s.tabs.len(), s.active), (2, 1));
    assert_ne!(s.generation, gen);
    assert_eq!(s.last_applied_seq, None);
    assert!(s.conversation.blocks.is_empty());
    assert_eq!(s.selected_model_id.as_deref(), Some("a"));
    assert_eq!(s.tabs[0].selected_model_id.as_deref(), Some("b"));
}

#[test]
fn resume_other_id_opens_a_tab_same_stopped_id_keeps_cursor() {
    let mut s = run("stopped");
    with_session(&mut s, "S1");
    let gen = s.generation.clone();
    let fx = handle_action(&mut s, &mut ui(), Action::ResumeSession("S1".into()));
    assert_eq!(fx, vec![Effect::Resume("S1".into())]);
    assert_eq!(
        (s.generation.clone(), s.last_applied_seq),
        (gen.clone(), Some(5))
    );
    assert!(!s.conversation.blocks.is_empty());
    let fx = resumed_now(&mut s, "S1").unwrap();
    assert!(fx.contains(&Effect::Subscribe {
        session_id: "S1".into(),
        cursor: Some(5)
    }));
    // another id: a new tab, fresh start
    let fx = handle_action(&mut s, &mut ui(), Action::ResumeSession("S2".into()));
    assert_eq!(fx.last(), Some(&Effect::FetchMetadata("S2".into())));
    assert_eq!((s.tabs.len(), s.active), (2, 1));
    assert_ne!(s.generation, gen);
    assert_eq!(s.last_applied_seq, None);
    let fx = resumed_now(&mut s, "S2").unwrap();
    assert!(fx.contains(&Effect::Subscribe {
        session_id: "S2".into(),
        cursor: None
    }));
    // S1 again: its tab comes forward, nothing is resumed
    let fx = handle_action(&mut s, &mut ui(), Action::ResumeSession("S1".into()));
    assert_eq!(s.active, 0);
    assert_eq!(s.activity, Activity::Idle);
    assert!(!fx.contains(&Effect::Resume("S1".into())), "{fx:?}");
}

#[test]
fn bug2_resync_keeps_catalog_model_sessions_pending_submit_and_notice() {
    let mut s = run("turn_replay");
    with_session(&mut s, "S1");
    s.catalog = catalog();
    s.selected_model_id = Some("a".into());
    s.sessions = vec![SessionListEntry {
        bypass: false,
        bypass_seq: 0,
        incarnation: None,
        read_only: false,
        resume_disabled_reason: None,
        id: "S0".into(),
        title: None,
        created_at: "t".into(),
        updated_at: "t".into(),
        message_count: 1,
        agent: None,
        live: false,
        parent_session_id: None,
        child_status: None,
        promptable: true,
    }];
    s.pending_submit_text = Some("draft".into());
    s.notice = Some("note".into());
    s.workspace = "/ws".into();
    let gen = s.generation.clone();
    let fx = update(
        &mut s,
        ClientEvent::Resync {
            generation: gen.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        fx,
        vec![Effect::Subscribe {
            session_id: "S1".into(),
            cursor: None
        }]
    );
    assert_eq!(s.generation, gen); // same stream replays: no bump
    assert_eq!(s.catalog.len(), 2);
    assert_eq!(s.selected_model_id.as_deref(), Some("a"));
    assert_eq!(s.sessions.len(), 1);
    assert_eq!(s.pending_submit_text.as_deref(), Some("draft"));
    assert_eq!(s.notice.as_deref(), Some("note"));
    assert_eq!(s.session.as_ref().unwrap().session_id, "S1");
    assert!(s.conversation.blocks.is_empty());
}

#[test]
fn bug3_create_and_resume_use_the_workspace() {
    let mut s = st();
    s.workspace = "/home/me/proj".into();
    let fx = handle_action(&mut s, &mut ui(), Action::Submit("hi".into()));
    match &fx[0] {
        Effect::Create(r) => assert_eq!(
            (r.workspace.as_str(), r.resume.as_deref()),
            ("/home/me/proj", None)
        ),
        other => panic!("{other:?}"),
    }
    created_now(&mut s, "S1").unwrap();
    assert_eq!(s.session.as_ref().unwrap().workspace, "/home/me/proj");
    let mut s2 = st();
    s2.workspace = "/home/me/proj".into();
    resumed_now(&mut s2, "S1").unwrap();
    assert_eq!(s2.session.as_ref().unwrap().workspace, "/home/me/proj");
}

#[test]
fn bug4_submit_revision_reaches_send_and_clear_editor() {
    let mut s = st();
    let fx = handle_action(
        &mut s,
        &mut ui(),
        Action::SubmitAt {
            text: "hi".into(),
            revision: 7,
        },
    );
    assert!(matches!(fx[0], Effect::Create(_)));
    let fx = created_now(&mut s, "S1").unwrap();
    let Effect::SendAt { request, revision } = fx.last().unwrap().clone() else {
        panic!("{fx:?}")
    };
    assert_eq!(revision, 7);
    assert_eq!(request.content.len(), 1);
    assert!(s.pending_submit_text.is_none());
    let ok = Ok(SubmitResponse {
        command_id: "c".into(),
        accepted: true,
        actor_state: "idle".into(),
        notice: None,
    });
    let fx = submitted_now(&mut s, 7, ok).unwrap();
    assert_eq!(fx, vec![Effect::ClearEditor { revision: 7 }]);
    // Submit(text) is SubmitAt with revision 0 and the reducer no longer invents revisions
    let fx = handle_action(&mut s, &mut ui(), Action::Submit("again".into()));
    assert!(matches!(fx[0], Effect::SendAt { revision: 0, .. }));
    assert_eq!(s.editor_revision, 0);
}

#[test]
fn bug5_post_call_keeps_args_and_name_and_exit_code_lands_on_its_own_block() {
    let s = run("tools_replay");
    let c2 = tool(&s, "c2");
    assert_eq!(c2.args, json!({"command":"ls"}));
    assert_eq!(c2.exit_code, Some(101));
    // c2 is not the last block (the final step follows) and c1 has no exit code
    assert!(matches!(
        s.conversation.blocks.last().unwrap().kind,
        BlockKind::AssistantStep { .. }
    ));
    assert_eq!(tool(&s, "c1").exit_code, None);
    // post-call without a name keeps the one already known
    let mut envs = load("tools_replay");
    for e in envs.iter_mut().filter(|e| e.kind == "tool.post_call") {
        e.payload.as_object_mut().unwrap().remove("name");
    }
    let mut s = st();
    feed(&mut s, &envs);
    assert_eq!(
        (tool(&s, "c1").name.as_str(), tool(&s, "c1").args),
        ("read", json!({"path":"a.rs"}))
    );
}

#[test]
fn bug6_interrupted_or_error_turn_settles_tools_and_planned_tools_do_not_run() {
    for outcome in ["interrupted", "error"] {
        let mut envs = load("interrupted_turn");
        envs.last_mut().unwrap().payload["outcome"] = json!(outcome);
        let mut s = st();
        feed(&mut s, &envs);
        for id in ["t1", "t2", "t3"] {
            assert_eq!(
                tool(&s, id).status,
                Some(ToolStatus::Interrupted),
                "{outcome} {id}"
            );
        }
        assert_eq!(s.activity, Activity::Idle);
    }
    // a done turn leaves a running tool alone but a never-pre-called plan is settled
    let mut envs = load("interrupted_turn");
    envs.last_mut().unwrap().payload["outcome"] = json!("done");
    let mut s = st();
    feed(&mut s, &envs);
    assert_eq!(tool(&s, "t1").status, None);
    assert_eq!(tool(&s, "t2").status, Some(ToolStatus::Interrupted));
}

#[test]
fn bug6_session_end_interrupts_unfinished_tools_and_tools_by_turn_uses_the_actor() {
    let mut envs = load("interrupted_turn");
    envs.pop(); // no turn.end
    let n = envs.len() as u64;
    let mut end = envs[0].clone();
    end.seq = n;
    end.kind = "session.end".into();
    end.payload = serde_json::to_value(SessionEnd {
        reason: "stop".into(),
        final_seq: n,
    })
    .unwrap();
    envs.push(end);
    let mut s = st();
    feed(&mut s, &envs);
    assert_eq!(s.activity, Activity::Stopped);
    assert_eq!(tool(&s, "t1").status, Some(ToolStatus::Interrupted));
    assert_eq!(tool(&s, "t3").status, Some(ToolStatus::Interrupted));
    // actor id comes from the envelope
    let mut envs = load("tools_replay");
    mutate(&mut envs, |e| e.actor_id = "agent:sub".into());
    let mut s = st();
    feed(&mut s, &envs);
    let keys: Vec<_> = s.conversation.tools_by_turn.keys().cloned().collect();
    assert_eq!(keys, vec![("agent:sub".to_string(), Ulid(100).to_string())]);
}

#[test]
fn bug7_local_block_ids_are_unique_and_stable() {
    let mut s = st();
    let g = s.generation.clone();
    for _ in 0..2 {
        update(
            &mut s,
            ClientEvent::Submitted {
                generation: g.clone(),
                revision: 1,
                result: Err("boom".into()),
            },
        )
        .unwrap();
        update(
            &mut s,
            ClientEvent::DeliveryUncertain {
                generation: g.clone(),
            },
        )
        .unwrap();
        let notice = Ok(SubmitResponse {
            command_id: "c".into(),
            accepted: true,
            actor_state: "idle".into(),
            notice: Some("downgraded".into()),
        });
        update(
            &mut s,
            ClientEvent::Submitted {
                generation: g.clone(),
                revision: 1,
                result: notice,
            },
        )
        .unwrap();
    }
    let all = ids(&s);
    assert_eq!(all.len(), 6);
    let mut dedup = all.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), 6);
    assert!(all.iter().all(|i| i.starts_with("local:")));
    assert_eq!(all[0], "local:1");
    // the counter survives a new conversation
    handle_action(&mut s, &mut ui(), Action::NewConversation);
    let g = s.generation.clone();
    update(&mut s, ClientEvent::DeliveryUncertain { generation: g }).unwrap();
    assert_eq!(ids(&s), vec!["local:7"]);
}

#[test]
fn bug8_turn_end_usage_fallback_works_on_replay() {
    // the last response of the turn reported no usage; turn.end carries it
    let s = run("usage_fallback");
    assert!(s.conversation.current_step.is_none()); // no deltas on replay
    assert_eq!(s.conversation.session_tokens, Some(110 + 170));
    assert_eq!(s.conversation.context_tokens, Some(150));
    assert_eq!(s.conversation.completion_tokens, Some(20));
    assert!(!s.conversation.missing_usage);
}

#[test]
fn bug9_metadata_refreshes_display_and_stale_metadata_cannot_clobber_a_pick() {
    let mut s = st();
    models_now(&mut s, Some("a".into())).unwrap();
    assert_eq!(
        (s.selected_model_name.as_deref(), s.context_window),
        (Some("a"), Some(1000))
    );
    with_session(&mut s, "S1");
    let old = s.generation.clone();
    handle_action(&mut s, &mut ui(), Action::PickModel("b".into()));
    let meta = |g: &Generation, model: &str| ClientEvent::Metadata {
        generation: g.clone(),
        result: Ok(SessionMetadata {
            bypass: false,
            bypass_seq: 0,
            incarnation: None,
            source: "config".into(),
            variant_source: "model_default".into(),
            read_only: false,
            resume_disabled_reason: None,
            session_id: "S1".into(),
            actor_state: "idle".into(),
            workspace: "/ws".into(),
            model: model.into(),
            agent: None,
            variant: None,
            parent_session_id: None,
            promptable: true,
            child_status: None,
            current_seq: 3,
        }),
    };
    update(&mut s, meta(&old, "a")).unwrap(); // stale: the old conversation's metadata
    assert_eq!(
        (
            s.selected_model_id.as_deref(),
            s.selected_model_name.as_deref(),
            s.context_window
        ),
        (Some("b"), Some("b"), None)
    );
    assert!(s.session.is_none());
    // current metadata refreshes name and window from the catalog
    let g = s.generation.clone();
    update(&mut s, meta(&g, "a")).unwrap();
    assert_eq!(
        (
            s.selected_model_id.as_deref(),
            s.selected_model_name.as_deref(),
            s.context_window
        ),
        (Some("a"), Some("a"), Some(1000))
    );
}

#[test]
fn bug10_tool_fragments_are_keyed_by_index_and_never_rendered() {
    let live = load("tools_live");
    let mut s = st();
    feed(&mut s, &live[..9]); // through the last tool-call fragment
    let mut keys: Vec<_> = s
        .conversation
        .pending_tool_fragments
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["0", "1"]);
    assert_eq!(
        s.conversation.pending_tool_fragments["0"],
        "{\"path\":\"a.rs\"}"
    );
    assert!(!s
        .conversation
        .blocks
        .iter()
        .any(|b| matches!(b.kind, BlockKind::ToolCall { .. })));
    feed(&mut s, &live[9..10]); // llm.response
    assert!(s.conversation.pending_tool_fragments.is_empty());
    let tools: Vec<_> = ids(&s)
        .into_iter()
        .filter(|i| i.starts_with("tool:"))
        .collect();
    assert_eq!(tools, vec!["tool:c1", "tool:c2"]);
}

#[test]
fn failed_turn_shows_an_error_block_identically_live_and_on_replay() {
    let (live, replay) = (run("error_live"), run("error_replay"));
    assert_eq!(live.conversation.blocks, replay.conversation.blocks);
    assert_eq!(live.activity, Activity::Idle);
    let Some(Block {
        kind: BlockKind::Error(text),
        ..
    }) = live.conversation.blocks.last()
    else {
        panic!("no error block: {:?}", live.conversation.blocks);
    };
    assert!(
        text.starts_with(&format!("provider: {QUOTA_MESSAGE}")),
        "{text}"
    );
    assert!(!text.contains('\n'));
    assert!(text.chars().count() <= 301, "{}", text.chars().count());
}

#[test]
fn interrupted_turn_pushes_a_short_notice_identically_live_and_on_replay() {
    let envs = load("interrupted_turn");
    let live = {
        let mut s = st();
        feed(&mut s, &envs);
        s
    };
    let replay = {
        let mut s = st();
        feed(&mut s, &without_deltas(&envs));
        s
    };
    assert_eq!(live.conversation.blocks, replay.conversation.blocks);
    assert_eq!(
        live.conversation.blocks.last().map(|b| b.kind.clone()),
        Some(BlockKind::Notice("turn interrupted".into()))
    );
}

#[test]
fn message_source_is_kept_as_the_actor_id() {
    let s = run("turn_live");
    assert!(matches!(
        &s.conversation.blocks[0].kind,
        BlockKind::Message { source, .. } if source == "user:http"
    ));
}

// ---------------------------------------------------------------- tabs

/// Opens a tab and gives it the conversation `id`, as a `Created` answer would.
fn open_with(s: &mut State, ui: &mut UiState, id: &str) {
    handle_action(s, ui, Action::NewConversation);
    created_now(s, id).unwrap();
}

fn notices(s: &State) -> Vec<String> {
    s.conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Notice(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn tabs_open_switch_and_close() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    open_with(&mut s, &mut ui, "S2");
    open_with(&mut s, &mut ui, "S3");
    assert_eq!((s.tabs.len(), s.active), (3, 2));
    let fx = handle_action(&mut s, &mut ui, Action::SwitchTab(0));
    assert_eq!(s.active, 0);
    assert_eq!(s.session.as_ref().unwrap().session_id, "S1");
    assert!(matches!(fx[..], [Effect::SetEditor { .. }]));
    // out of range or the active tab: nothing happens
    assert!(handle_action(&mut s, &mut ui, Action::SwitchTab(5)).is_empty());
    assert!(handle_action(&mut s, &mut ui, Action::SwitchTab(0)).is_empty());
    // closing stops nothing on the server and says so
    handle_action(&mut s, &mut ui, Action::SwitchTab(1));
    let fx = handle_action(&mut s, &mut ui, Action::CloseTab);
    assert!(!fx.iter().any(|e| matches!(e, Effect::Stop(_))), "{fx:?}");
    assert_eq!(s.tabs.len(), 2);
    assert_eq!(s.session.as_ref().unwrap().session_id, "S3");
    let shown = notices(&s);
    assert!(
        shown.last().unwrap().contains("the conversation is kept"),
        "{shown:?}"
    );
    // closing the last tab leaves a blank one
    handle_action(&mut s, &mut ui, Action::CloseTab);
    handle_action(&mut s, &mut ui, Action::CloseTab);
    assert_eq!(s.tabs.len(), 1);
    assert!(s.session.is_none());
}

#[test]
fn closing_a_running_tab_says_its_turn_goes_on() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    s.activity = Activity::Running;
    open_with(&mut s, &mut ui, "S2");
    handle_action(&mut s, &mut ui, Action::SwitchTab(0));
    handle_action(&mut s, &mut ui, Action::CloseTab);
    assert_eq!(s.session.as_ref().unwrap().session_id, "S2");
    let shown = notices(&s);
    assert!(
        shown
            .last()
            .unwrap()
            .contains("keeps running on the server"),
        "{shown:?}"
    );
}

#[test]
fn a_tenth_tab_is_refused_with_a_notice() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    for i in 2..=MAX_TABS {
        open_with(&mut s, &mut ui, &format!("S{i}"));
    }
    assert_eq!(s.tabs.len(), MAX_TABS);
    let fx = handle_action(&mut s, &mut ui, Action::NewConversation);
    assert!(fx.is_empty());
    assert_eq!((s.tabs.len(), s.active), (MAX_TABS, MAX_TABS - 1));
    assert!(notices(&s).last().unwrap().contains("close one"));
    let fx = handle_action(&mut s, &mut ui, Action::ResumeSession("other".into()));
    assert!(fx.is_empty(), "{fx:?}");
    // an already open conversation is still focused
    handle_action(&mut s, &mut ui, Action::ResumeSession("S1".into()));
    assert_eq!(s.active, 0);
}

#[test]
fn events_reach_their_tab_and_a_background_tab_is_unread() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let first = s.generation.clone();
    open_with(&mut s, &mut ui, "S2");
    let second = s.generation.clone();
    assert_ne!(first, second);
    // S1's stream delivers while S2 is on screen
    for e in load("turn_replay") {
        update(
            &mut s,
            ClientEvent::Envelope {
                generation: first.clone(),
                envelope: e,
            },
        )
        .unwrap();
    }
    assert!(
        s.conversation.blocks.is_empty(),
        "the active tab is untouched"
    );
    assert!(!s.unread);
    assert!(s.tabs[0].unread);
    assert_eq!(s.tabs[0].last_applied_seq, Some(8));
    assert_eq!(s.tabs[0].activity, Activity::Idle);
    // stream status lands on its tab too
    update(&mut s, ClientEvent::Reconnecting { generation: first }).unwrap();
    assert_eq!(s.tabs[0].connection, ConnectionStatus::Reconnecting);
    assert_eq!(s.connection, ConnectionStatus::Disconnected);
    // the active tab's events are not unread
    feed(&mut s, &load("turn_replay"));
    assert!(!s.unread);
    // coming forward clears the mark
    handle_action(&mut s, &mut ui, Action::SwitchTab(0));
    assert!(!s.unread);
    assert!(!s.conversation.blocks.is_empty());
}

#[test]
fn a_closed_tab_drops_its_late_events() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    open_with(&mut s, &mut ui, "S2");
    let gone = s.generation.clone();
    handle_action(&mut s, &mut ui, Action::CloseTab);
    let before = s.clone();
    let fx = update(
        &mut s,
        ClientEvent::Envelope {
            generation: gone,
            envelope: load("turn_replay")[1].clone(),
        },
    )
    .unwrap();
    assert!(fx.is_empty());
    assert_eq!(s, before);
}

#[test]
fn a_background_creation_subscribes_for_its_own_tab() {
    let (mut s, mut ui) = (st(), ui());
    handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
    let first = s.generation.clone();
    // the Captain opens another tab before the server answers
    handle_action(&mut s, &mut ui, Action::NewConversation);
    assert_eq!(s.active, 1);
    let fx = update(
        &mut s,
        ClientEvent::Created {
            generation: first,
            result: Ok(CreateSessionResponse {
                session_id: "S1".into(),
                root_actor_id: "agent:main".into(),
                actor_state: "idle".into(),
            }),
        },
    )
    .unwrap();
    assert!(fx.contains(&Effect::Subscribe {
        session_id: "S1".into(),
        cursor: None
    }));
    assert!(fx.iter().any(|e| matches!(e, Effect::SendAt { .. })));
    assert_eq!(s.tabs[0].session.as_ref().unwrap().session_id, "S1");
    assert!(s.session.is_none(), "the tab on screen stays blank");
}

#[test]
fn drafts_and_positions_are_kept_per_tab() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    s.draft = "half a question".into();
    s.draft_revision = 4;
    ui.scroll = 7;
    ui.follow_bottom = false;
    let fx = handle_action(&mut s, &mut ui, Action::NewConversation);
    assert_eq!(
        fx,
        vec![
            Effect::SetEditor {
                text: String::new(),
                revision: 0
            },
            Effect::FetchAgents(None)
        ]
    );
    assert!(ui.follow_bottom);
    assert_eq!(ui.scroll, 0);
    s.draft = "other".into();
    s.draft_revision = 2;
    let fx = handle_action(&mut s, &mut ui, Action::SwitchTab(0));
    assert_eq!(
        fx,
        vec![Effect::SetEditor {
            text: "half a question".into(),
            revision: 4
        }]
    );
    assert_eq!((ui.scroll, ui.follow_bottom), (7, false));
    assert_eq!(s.tabs[1].draft, "other");
}

#[test]
fn a_send_accepted_in_the_background_clears_that_tabs_draft_only() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let first = s.generation.clone();
    s.draft = "sent text".into();
    s.draft_revision = 9;
    handle_action(&mut s, &mut ui, Action::NewConversation);
    let fx = update(
        &mut s,
        ClientEvent::Submitted {
            generation: first,
            revision: 9,
            result: Ok(SubmitResponse {
                command_id: "c".into(),
                accepted: true,
                actor_state: "running".into(),
                notice: None,
            }),
        },
    )
    .unwrap();
    assert!(fx.is_empty(), "the editor on screen belongs to another tab");
    assert!(s.tabs[0].draft.is_empty());
}

#[test]
fn sidebar_toggle() {
    let (mut s, mut ui) = (st(), ui());
    handle_action(&mut s, &mut ui, Action::ToggleSidebar);
    assert!(ui.sidebar_hidden);
    handle_action(&mut s, &mut ui, Action::ToggleSidebar);
    assert!(!ui.sidebar_hidden);
}

// ---------------------------------------------------------------- context gauge

/// The `FetchContext` the reducer issues for `id` under the conversation's current epoch.
fn fetch(id: &str, s: &State) -> Effect {
    Effect::FetchContext {
        session_id: id.into(),
        epoch: s.conversation.context_epoch,
    }
}

fn of_incarnation(n: u128, mut snapshot: ContextSnapshot) -> ContextSnapshot {
    snapshot.incarnation = Ulid(n);
    snapshot
}

fn snapshot(revision: u64, estimate_total: u64) -> ContextSnapshot {
    ContextSnapshot {
        gateway: "gateway".into(),
        upstream_id: "upstream".into(),
        model: "default".into(),
        variant: None,
        window: 1_000_000,
        window_source: "models.json".into(),

        estimate_total,
        breakdown: Default::default(),
        output_reserve: 8192,
        margin: 50_000,
        input_budget: 941_808,
        percent: estimate_total * 100 / 1_000_000,
        last_usage: None,
        skills: Default::default(),
        rule_files: 0,
        incarnation: Ulid(1),
        revision,
    }
}

#[test]
fn context_updated_feeds_the_gauge_and_keeps_the_provider_count_apart() {
    let mut s = run("tools_replay");
    assert_eq!(s.conversation.context_tokens, Some(150));
    assert!(s.conversation.context.is_none());
    let mut script = Script::new();
    script.seq = 1_000;
    script.push(EventKind::ContextUpdated, &snapshot(2, 42_000));
    feed(&mut s, &script.out);
    assert_eq!(
        s.conversation.context.as_ref().unwrap().estimate_total,
        42_000
    );
    assert_eq!(s.conversation.context_tokens, Some(150));
}

#[test]
fn an_older_snapshot_never_replaces_a_newer_one() {
    let mut s = st();
    let mut script = Script::new();
    script.push(EventKind::ContextUpdated, &snapshot(5, 50_000));
    script.push(EventKind::ContextUpdated, &snapshot(4, 10_000));
    feed(&mut s, &script.out);
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 5);
    let ev = ClientEvent::Context {
        generation: s.generation.clone(),
        epoch: s.conversation.context_epoch,
        result: Ok(snapshot(6, 60_000)),
    };
    update(&mut s, ev).unwrap();
    assert_eq!(
        s.conversation.context.as_ref().unwrap().estimate_total,
        60_000
    );
    // An error (an older server has no endpoint) changes nothing.
    let ev = ClientEvent::Context {
        generation: s.generation.clone(),
        epoch: s.conversation.context_epoch,
        result: Err("HTTP 404".into()),
    };
    update(&mut s, ev).unwrap();
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 6);
}

#[test]
fn a_malformed_context_payload_is_a_protocol_error() {
    let mut s = st();
    let mut script = Script::new();
    script.raw("context.updated", json!({"window": "big"}));
    let ev = envelope(&s, &script.out[0]);
    assert!(update(&mut s, ev).is_err());
}

#[test]
fn every_stream_connection_reads_the_context_back() {
    let mut s = st();
    models_now(&mut s, Some("a".into())).unwrap();
    // Before a session exists there is nothing to fetch.
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    assert!(update(&mut s, connected).unwrap().is_empty());
    let created = ClientEvent::Created {
        generation: s.generation.clone(),
        result: Ok(CreateSessionResponse {
            session_id: "s1".into(),
            root_actor_id: "agent:main".into(),
            actor_state: "idle".into(),
        }),
    };
    let effects = update(&mut s, created).unwrap();
    assert!(!effects.contains(&fetch("s1", &s)));
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    let effects = update(&mut s, connected).unwrap();
    assert_eq!(effects, vec![fetch("s1", &s)]);
}

#[test]
fn a_snapshot_missed_during_a_disconnect_is_read_back_on_reconnect() {
    let mut s = st();
    models_now(&mut s, Some("a".into())).unwrap();
    let created = ClientEvent::Created {
        generation: s.generation.clone(),
        result: Ok(CreateSessionResponse {
            session_id: "s1".into(),
            root_actor_id: "agent:main".into(),
            actor_state: "idle".into(),
        }),
    };
    update(&mut s, created).unwrap();
    let mut script = Script::new();
    script.seq = 1_000;
    script.push(EventKind::ContextUpdated, &snapshot(2, 20_000));
    feed(&mut s, &script.out);
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 2);

    // The stream drops; revision 3 is published while nobody listens.
    for event in [
        ClientEvent::StreamDisconnected {
            generation: s.generation.clone(),
        },
        ClientEvent::Reconnecting {
            generation: s.generation.clone(),
        },
    ] {
        update(&mut s, event).unwrap();
    }
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    let effects = update(&mut s, connected).unwrap();
    assert_eq!(effects, vec![fetch("s1", &s)]);
    let fetched = ClientEvent::Context {
        generation: s.generation.clone(),
        epoch: s.conversation.context_epoch,
        result: Ok(snapshot(3, 30_000)),
    };
    update(&mut s, fetched).unwrap();
    let held = s.conversation.context.as_ref().unwrap();
    assert_eq!((held.revision, held.estimate_total), (3, 30_000));
}

#[test]
fn a_resync_keeps_the_gauge_until_the_reconnect_refreshes_it() {
    let mut s = st();
    let held = ClientEvent::Context {
        generation: s.generation.clone(),
        epoch: s.conversation.context_epoch,
        result: Ok(snapshot(3, 30_000)),
    };
    update(&mut s, held).unwrap();
    let resync = ClientEvent::Resync {
        generation: s.generation.clone(),
    };
    update(&mut s, resync).unwrap();
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 3);
}

#[test]
fn a_resumed_session_s_snapshots_replace_the_previous_incarnations() {
    let mut s = st();
    let held = ClientEvent::Context {
        generation: s.generation.clone(),
        epoch: s.conversation.context_epoch,
        result: Ok(snapshot(40, 400_000)),
    };
    update(&mut s, held).unwrap();
    // Same id, stopped and resumed: a new actor starts its revisions again.
    let mut resumed = snapshot(0, 12_000);
    resumed.incarnation = Ulid(2);
    let fetched = ClientEvent::Context {
        generation: s.generation.clone(),
        epoch: s.conversation.context_epoch,
        result: Ok(resumed.clone()),
    };
    update(&mut s, fetched).unwrap();
    assert_eq!(s.conversation.context.as_ref(), Some(&resumed));
    // Within the new incarnation the revision order applies again.
    let mut script = Script::new();
    script.seq = 1_000;
    let mut next = snapshot(1, 13_000);
    next.incarnation = Ulid(2);
    let mut late = snapshot(0, 1);
    late.incarnation = Ulid(2);
    script.push(EventKind::ContextUpdated, &next);
    script.push(EventKind::ContextUpdated, &late);
    feed(&mut s, &script.out);
    assert_eq!(s.conversation.context.as_ref().unwrap().revision, 1);
}

fn live_context(s: &mut State, snapshot: &ContextSnapshot) {
    let mut script = Script::new();
    script.seq = s.last_applied_seq.map_or(1_000, |seq| seq + 1);
    script.push(EventKind::ContextUpdated, snapshot);
    feed(s, &script.out);
}

fn shown(s: &State) -> (u128, u64) {
    let c = s.conversation.context.as_ref().unwrap();
    (c.incarnation.0, c.revision)
}

#[test]
fn a_retired_incarnation_is_never_shown_again_by_a_late_fetch_or_a_late_event() {
    let mut s = st();
    let a = of_incarnation(1, snapshot(40, 400_000));
    let b = of_incarnation(2, snapshot(0, 12_000));
    let epoch = s.conversation.context_epoch;
    let answer = |epoch, snapshot: &ContextSnapshot, s: &State| ClientEvent::Context {
        generation: s.generation.clone(),
        epoch,
        result: Ok(snapshot.clone()),
    };
    let ev = answer(epoch, &a, &s);
    update(&mut s, ev).unwrap();
    assert_eq!(shown(&s), (1, 40));
    let ev = answer(epoch, &b, &s);
    update(&mut s, ev).unwrap();
    assert_eq!(shown(&s), (2, 0));
    // A slow answer about A (same epoch), then a late live event about A.
    let ev = answer(epoch, &a, &s);
    update(&mut s, ev).unwrap();
    assert_eq!(shown(&s), (2, 0));
    live_context(&mut s, &of_incarnation(1, snapshot(41, 410_000)));
    assert_eq!(shown(&s), (2, 0));
    // B keeps moving on its own revisions.
    live_context(&mut s, &of_incarnation(2, snapshot(3, 15_000)));
    assert_eq!(shown(&s), (2, 3));
    live_context(&mut s, &of_incarnation(2, snapshot(2, 14_000)));
    assert_eq!(shown(&s), (2, 3));
}

#[test]
fn the_retired_set_is_bounded_and_survives_a_resync() {
    let mut s = st();
    for n in 1..=12u128 {
        live_context(&mut s, &of_incarnation(n, snapshot(0, 1_000)));
    }
    assert_eq!(shown(&s), (12, 0));
    assert_eq!(s.conversation.retired_incarnations.len(), 8);
    let resync = ClientEvent::Resync {
        generation: s.generation.clone(),
    };
    update(&mut s, resync).unwrap();
    assert_eq!(s.conversation.retired_incarnations.len(), 8);
    live_context(&mut s, &of_incarnation(11, snapshot(9, 9_000)));
    assert_eq!(shown(&s), (12, 0));
}

#[test]
fn a_fetch_issued_before_a_resume_and_answered_after_it_is_dropped() {
    let mut s = st();
    models_now(&mut s, Some("a".into())).unwrap();
    let created = |s: &State, kind_resumed: bool| {
        let result = Ok(CreateSessionResponse {
            session_id: "s1".into(),
            root_actor_id: "agent:main".into(),
            actor_state: "idle".into(),
        });
        if kind_resumed {
            ClientEvent::Resumed {
                generation: s.generation.clone(),
                result,
            }
        } else {
            ClientEvent::Created {
                generation: s.generation.clone(),
                result,
            }
        }
    };
    let ev = created(&s, false);
    update(&mut s, ev).unwrap();
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    let effects = update(&mut s, connected).unwrap();
    let Effect::FetchContext { epoch: old, .. } = effects[0].clone() else {
        panic!("{effects:?}")
    };
    // The session is stopped and resumed under the same id before the answer arrives.
    let ev = created(&s, true);
    update(&mut s, ev).unwrap();
    assert!(s.conversation.context_epoch > old);
    let late = ClientEvent::Context {
        generation: s.generation.clone(),
        epoch: old,
        result: Ok(of_incarnation(1, snapshot(40, 400_000))),
    };
    update(&mut s, late).unwrap();
    assert!(s.conversation.context.is_none());
    // The fetch of the new epoch is accepted.
    let connected = ClientEvent::StreamConnected {
        generation: s.generation.clone(),
    };
    let effects = update(&mut s, connected).unwrap();
    assert_eq!(effects, vec![fetch("s1", &s)]);
    let fresh = ClientEvent::Context {
        generation: s.generation.clone(),
        epoch: s.conversation.context_epoch,
        result: Ok(of_incarnation(2, snapshot(0, 12_000))),
    };
    update(&mut s, fresh).unwrap();
    assert_eq!(shown(&s), (2, 0));
}

#[test]
fn a_background_tabs_context_update_reaches_that_tab_only_and_shows_on_switch() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let first = s.generation.clone();
    open_with(&mut s, &mut ui, "S2");
    let second = s.generation.clone();
    // the active tab already shows its own estimate
    let epoch = s.conversation.context_epoch;
    update(
        &mut s,
        ClientEvent::Context {
            generation: second.clone(),
            epoch,
            result: Ok(snapshot(1, 10_000)),
        },
    )
    .unwrap();
    // the background tab's stream reports a new estimate
    let mut script = Script::new();
    script.seq = 500;
    script.push(EventKind::ContextUpdated, &snapshot(3, 77_000));
    update(
        &mut s,
        ClientEvent::Envelope {
            generation: first.clone(),
            envelope: script.out[0].clone(),
        },
    )
    .unwrap();
    assert_eq!(s.active, 1);
    assert_eq!(
        s.conversation.context.as_ref().unwrap().estimate_total,
        10_000,
        "the gauge on screen is unchanged"
    );
    assert!(!s.tabs[0].unread, "a gauge refresh is not news");
    // its fetch answer, under its own epoch, lands on it too
    let epoch = s.tabs[0].conversation.context_epoch;
    update(
        &mut s,
        ClientEvent::Context {
            generation: first,
            epoch,
            result: Ok(snapshot(4, 80_000)),
        },
    )
    .unwrap();
    assert_eq!(
        s.conversation.context.as_ref().unwrap().estimate_total,
        10_000
    );
    handle_action(&mut s, &mut ui, Action::SwitchTab(0));
    assert_eq!(
        s.conversation.context.as_ref().unwrap().estimate_total,
        80_000,
        "switching shows the background tab's gauge"
    );
    // a closed tab's late answer is dropped
    handle_action(&mut s, &mut ui, Action::SwitchTab(1));
    handle_action(&mut s, &mut ui, Action::CloseTab);
    let before = s.clone();
    update(
        &mut s,
        ClientEvent::Context {
            generation: second,
            epoch: 0,
            result: Ok(snapshot(9, 99_000)),
        },
    )
    .unwrap();
    assert_eq!(s, before);
}

#[test]
fn each_tab_keeps_its_own_context_epoch() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let first = s.generation.clone();
    open_with(&mut s, &mut ui, "S2");
    let before = s.tabs[0].conversation.context_epoch;
    // the active tab reconnects: only its epoch moves, and its fetch names its session
    let generation = s.generation.clone();
    let fx = update(&mut s, ClientEvent::StreamConnected { generation }).unwrap();
    assert_eq!(fx, vec![fetch("S2", &s)]);
    assert_eq!(s.tabs[0].conversation.context_epoch, before);
    // the background tab reconnects: its fetch names its own session
    let fx = update(&mut s, ClientEvent::StreamConnected { generation: first }).unwrap();
    assert_eq!(
        fx,
        vec![Effect::FetchContext {
            session_id: "S1".into(),
            epoch: s.tabs[0].conversation.context_epoch,
        }]
    );
    assert_eq!(s.tabs[0].conversation.context_epoch, before + 1);
}

// ---------------------------------------------------------------- a resume still in flight

/// A tab that asked to resume `id` and has no answer yet.
fn resuming(s: &mut State, ui: &mut UiState, id: &str) -> Generation {
    let fx = handle_action(s, ui, Action::ResumeSession(id.into()));
    assert!(fx.contains(&Effect::FetchMetadata(id.into())), "{fx:?}");
    s.generation.clone()
}

#[test]
fn a_tab_waiting_for_a_resume_is_not_reused_by_a_new_conversation() {
    let (mut s, mut ui) = (st(), ui());
    resuming(&mut s, &mut ui, "S1");
    assert_eq!(s.resuming.as_deref(), Some("S1"));
    assert!(!s.is_blank());
    handle_action(&mut s, &mut ui, Action::NewConversation);
    assert_eq!((s.tabs.len(), s.active), (2, 1));
    assert_eq!(s.tabs[0].resuming.as_deref(), Some("S1"));
}

#[test]
fn choosing_a_conversation_being_resumed_focuses_its_tab() {
    let (mut s, mut ui) = (st(), ui());
    resuming(&mut s, &mut ui, "S1");
    handle_action(&mut s, &mut ui, Action::NewConversation);
    let fx = handle_action(&mut s, &mut ui, Action::ResumeSession("S1".into()));
    assert_eq!((s.tabs.len(), s.active), (2, 0));
    assert!(
        !fx.contains(&Effect::Resume("S1".into())),
        "asked once: {fx:?}"
    );
}

#[test]
fn a_message_sent_during_a_resume_reaches_the_resumed_conversation() {
    let (mut s, mut ui) = (st(), ui());
    let generation = resuming(&mut s, &mut ui, "S1");
    let fx = handle_action(
        &mut s,
        &mut ui,
        Action::SubmitAt {
            text: "hello again".into(),
            revision: 3,
        },
    );
    assert!(fx.is_empty(), "no new conversation is created: {fx:?}");
    let fx = update(
        &mut s,
        ClientEvent::Resumed {
            generation,
            result: Ok(CreateSessionResponse {
                session_id: "S1".into(),
                root_actor_id: "agent:main".into(),
                actor_state: "idle".into(),
            }),
        },
    )
    .unwrap();
    assert!(s.resuming.is_none());
    assert_eq!(s.session.as_ref().unwrap().session_id, "S1");
    let sent = fx
        .iter()
        .find_map(|e| match e {
            Effect::SendAt { request, revision } => Some((request.clone(), *revision)),
            _ => None,
        })
        .expect("the queued message is sent");
    assert_eq!(sent.1, 3);
    assert!(matches!(
        &sent.0.content[..],
        [withnav::bus::command::ContentBlock::Text { text }] if text == "hello again"
    ));
    // a second message while the first waited would have been refused, not lost silently
    let (mut s, mut ui) = (st(), self::ui());
    resuming(&mut s, &mut ui, "S2");
    handle_action(&mut s, &mut ui, Action::Submit("one".into()));
    assert!(handle_action(&mut s, &mut ui, Action::Submit("two".into())).is_empty());
    assert_eq!(s.pending_submit_text.as_deref(), Some("one"));
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Notice(n) if n.contains("still starting")
    ));
}

#[test]
fn a_failed_resume_frees_the_tab() {
    let (mut s, mut ui) = (st(), ui());
    let generation = resuming(&mut s, &mut ui, "S1");
    update(
        &mut s,
        ClientEvent::Resumed {
            generation,
            result: Err("HTTP 404".into()),
        },
    )
    .unwrap();
    assert!(s.resuming.is_none());
    assert!(s.is_blank());
}

#[test]
fn a_catalog_answer_reaches_every_tab() {
    let (mut s, mut ui) = (st(), ui());
    // tab 1 chose `a` before any catalog (no name, no window yet); tab 2 chose nothing
    s.selected_model_id = Some("a".into());
    created_now(&mut s, "S1").unwrap();
    handle_action(&mut s, &mut ui, Action::NewConversation);
    s.selected_model_id = None;
    s.selected_model_name = None;
    handle_action(&mut s, &mut ui, Action::SwitchTab(0));
    assert_eq!(s.selected_model_name, None);
    models_now(&mut s, Some("b".into())).unwrap();
    let background = &s.tabs[1];
    assert_eq!(
        background.selected_model_id, None,
        "catalog alone does not invent an agent assignment"
    );
    assert_eq!(background.selected_model_name, None);
    assert_eq!(
        s.selected_model_id.as_deref(),
        Some("a"),
        "a choice is kept"
    );
    assert_eq!(
        (s.selected_model_name.as_deref(), s.context_window),
        (Some("a"), Some(1000))
    );
    // a later catalog refreshes the background tab's window too
    let mut models = catalog();
    models[1].context_window = Some(2000);
    let generation = s.generation.clone();
    update(
        &mut s,
        ClientEvent::Models {
            workspace: "/ws".into(),
            request: 0,
            generation,
            result: Ok(ModelsBody {
                models,
                default_agent: None,
            }),
        },
    )
    .unwrap();
    assert_eq!(s.tabs[1].context_window, None);
}

// Given a tab whose conversation is still being resumed, holding a message the Captain sent
// When the Captain closes the tab, then the server's late answer arrives
// Then the close is refused with a notice and the tab stays, so the held message is sent
//      to the resumed conversation instead of being lost
#[test]
fn a_tab_holding_a_message_for_a_pending_resume_cannot_be_closed() {
    let (mut s, mut ui) = (st(), ui());
    let generation = resuming(&mut s, &mut ui, "S1");
    handle_action(&mut s, &mut ui, Action::Submit("keep me".into()));
    let tabs = s.tabs.len();
    assert!(handle_action(&mut s, &mut ui, Action::CloseTab).is_empty());
    assert_eq!(s.tabs.len(), tabs, "the tab is not closed");
    assert_eq!(s.pending_submit_text.as_deref(), Some("keep me"));
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Notice(n) if n.contains("waiting for the server")
    ));
    let fx = update(
        &mut s,
        ClientEvent::Resumed {
            generation,
            result: Ok(CreateSessionResponse {
                session_id: "S1".into(),
                root_actor_id: "agent:main".into(),
                actor_state: "idle".into(),
            }),
        },
    )
    .unwrap();
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::SendAt { request, .. }
            if matches!(&request.content[..], [withnav::bus::command::ContentBlock::Text { text }] if text == "keep me")
    )));
}

#[test]
fn context_compacted_pushes_a_one_line_notice() {
    let mut script = Script::new();
    script.start().push(
        EventKind::ContextCompacted,
        &ContextCompacted {
            written_by: None,
            passes: 1,
            generation: 1,
            through_message: 7,
            pruned: 2,
            estimate_before: 120_000,
            estimate_after: 18_000,
            reason: None,
        },
    );
    let mut s = st();
    feed(&mut s, &script.out);
    assert_eq!(
        s.conversation.blocks.last().map(|b| b.kind.clone()),
        Some(BlockKind::Notice("context compacted: ~120k -> ~18k".into()))
    );
    let kept = s
        .conversation
        .last_compaction
        .as_ref()
        .expect("kept for the panel");
    assert_eq!((kept.generation, kept.estimate_after), (1, 18_000));
    // a million tokens reads as the gauge writes it
    let mut script = Script::new();
    script.start().push(
        EventKind::ContextCompacted,
        &ContextCompacted {
            written_by: None,
            passes: 1,
            generation: 2,
            through_message: 70,
            pruned: 0,
            estimate_before: 1_000_000,
            estimate_after: 1_500,
            reason: None,
        },
    );
    let mut s = st();
    feed(&mut s, &script.out);
    assert_eq!(
        s.conversation.blocks.last().map(|b| b.kind.clone()),
        Some(BlockKind::Notice("context compacted: ~1M -> ~2k".into()))
    );
}

// ---------------------------------------------------------------- server commands

#[test]
fn typed_lines_route_like_the_server_with_client_commands_kept_local() {
    use withnav::tui::commands::{route, Routed};
    let server = |name: &str, arguments: &str| Routed::Server {
        name: name.into(),
        arguments: arguments.into(),
    };
    let mode = Mode::NextTurn;
    assert_eq!(route("/context", mode), server("context", ""));
    assert_eq!(route("/agent plan ", mode), server("agent", "plan"));
    assert_eq!(route("/variant high", mode), server("variant", "high"));
    assert_eq!(
        route("/compact keep the tests", mode),
        server("compact", "keep the tests")
    );
    assert_eq!(
        route("/nope", mode),
        server("nope", ""),
        "the server names close ones"
    );
    assert_eq!(route("//context", mode), Routed::Message("/context".into()));
    assert_eq!(
        route("hello /context", mode),
        Routed::Message("hello /context".into())
    );
    assert_eq!(route("/", mode), Routed::Message("/".into()));
    assert_eq!(
        route("/sessions", mode),
        Routed::Local(Action::OpenOverlay(Overlay::Sessions))
    );
    assert_eq!(
        route("/theme", mode),
        Routed::Local(Action::OpenOverlay(Overlay::Themes))
    );
    assert_eq!(
        route("/help", mode),
        Routed::Local(Action::OpenOverlay(Overlay::Help))
    );
    assert_eq!(route("/quit", mode), Routed::Local(Action::Quit));
    assert_eq!(route("/new", mode), Routed::Local(Action::NewConversation));
    assert_eq!(
        route("/new fast", mode),
        Routed::Local(Action::PickAgent("fast".into()))
    );
}

#[test]
fn a_command_runs_in_the_active_conversation_and_clears_the_editor_once_taken() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let action = Action::RunCommand {
        name: "context".into(),
        arguments: String::new(),
        revision: Some(4),
    };
    let fx = handle_action(&mut s, &mut ui, action);
    assert_eq!(
        fx,
        vec![Effect::RunCommand {
            session_id: "S1".into(),
            request: CommandRequest {
                name: "context".into(),
                arguments: String::new(),
            },
            revision: Some(4),
            token: 1,
        }]
    );
    let taken = ClientEvent::CommandRan {
        generation: s.generation.clone(),
        name: "context".into(),
        revision: Some(4),
        token: 1,
        result: Ok(CommandResponse {
            operation_id: "op".into(),
            ok: true,
            data: None,
            error: None,
        }),
    };
    assert_eq!(
        update(&mut s, taken).unwrap(),
        vec![Effect::ClearEditor { revision: 4 }]
    );
    // a request that never reached the server is shown and keeps the editor
    let lost = ClientEvent::CommandRan {
        generation: s.generation.clone(),
        name: "context".into(),
        revision: Some(5),
        token: 1,
        result: Err("HTTP 503".into()),
    };
    assert!(update(&mut s, lost).unwrap().is_empty());
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Command { name, ok: false, text } if name == "context" && text == "HTTP 503"
    ));
}

#[test]
fn a_command_without_a_live_conversation_is_answered_locally() {
    let (mut s, mut ui) = (st(), ui());
    let action = || Action::RunCommand {
        name: "context".into(),
        arguments: String::new(),
        revision: None,
    };
    assert!(handle_action(&mut s, &mut ui, action()).is_empty());
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Command { ok: false, text, .. } if text.contains("send a message first")
    ));
    created_now(&mut s, "S1").unwrap();
    s.activity = Activity::Stopped;
    assert!(handle_action(&mut s, &mut ui, action()).is_empty());
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Command { ok: false, text, .. } if text.contains("stopped")
    ));
}

#[test]
fn command_results_show_as_compact_blocks_live_and_on_replay() {
    let mut script = Script::new();
    script.start();
    script.raw(
        "command.result",
        json!({"operation_id": Ulid(7).to_string(), "name": "variant", "ok": true,
               "data": {"variant": "high", "offered": ["low", "high"], "applies": "next_turn"}}),
    );
    script.raw(
        "command.result",
        json!({"operation_id": Ulid(8).to_string(), "name": "agent", "ok": false,
               "error": "the agent is working: wait for the turn to end or stop it, then switch"}),
    );
    script.raw(
        "command.result",
        json!({"operation_id": Ulid(9).to_string(), "name": "context", "ok": true,
               "data": {"estimate_total": 42_000, "window": 1_000_000, "percent": 4,
                        "input_budget": 941_808,
                        "breakdown": {"system": 3000, "rules": 0, "skills_catalog": 0,
                                      "tools": 9000, "messages": 30_000}}}),
    );
    let mut s = st();
    feed(&mut s, &script.out);
    let shown: Vec<(String, bool, String)> = s
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Command { name, ok, text } => Some((name.clone(), *ok, text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert_eq!(
        shown[0],
        (
            "variant".into(),
            true,
            "variant high, applies next turn; on offer: low, high".into()
        )
    );
    assert!(!shown[1].1);
    assert!(shown[1].2.starts_with("the agent is working"));
    assert!(
        shown[2].2.starts_with("~42k / 1M · 4% · input budget 942k"),
        "{}",
        shown[2].2
    );
    assert!(shown[2].2.contains("messages"));
    // replaying the same events gives the same blocks
    let mut again = st();
    feed(&mut again, &script.out);
    assert_eq!(again.conversation.blocks, s.conversation.blocks);
}

#[test]
fn the_server_command_list_is_kept() {
    let mut s = st();
    let ev = ClientEvent::Commands {
        generation: Generation(12345),
        result: Ok(CommandsBody {
            commands: vec![ServerCommand {
                name: "context".into(),
                description: "Show how the context window is used".into(),
                args_hint: String::new(),
            }],
        }),
    };
    update(&mut s, ev).unwrap();
    assert_eq!(
        s.server_commands.len(),
        1,
        "shared: kept whatever its generation"
    );
}

// ---------------------------------------------------------------- build / plan

#[test]
fn toggling_the_agent_before_a_conversation_sets_the_one_it_starts_as() {
    let (mut s, mut ui) = (st(), ui());
    assert_eq!(s.agent_name(), "build");
    assert_eq!(
        handle_action(&mut s, &mut ui, Action::ToggleAgent),
        vec![Effect::FetchAgents(None)]
    );
    assert_eq!(s.agent_name(), "plan");
    let fx = handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
    assert!(
        matches!(&fx[0], Effect::Create(r) if r.agent.as_deref() == Some("plan")),
        "{fx:?}"
    );
    let mut back = st();
    handle_action(&mut back, &mut ui, Action::ToggleAgent);
    handle_action(&mut back, &mut ui, Action::ToggleAgent);
    assert_eq!(
        back.agent.as_deref(),
        Some("build"),
        "explicit build is not the server default"
    );
}

#[test]
fn toggling_the_agent_of_a_conversation_goes_through_the_server() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let fx = handle_action(&mut s, &mut ui, Action::ToggleAgent);
    assert_eq!(
        fx,
        vec![Effect::RunCommand {
            session_id: "S1".into(),
            request: CommandRequest {
                name: "agent".into(),
                arguments: "plan".into(),
            },
            revision: None,
            token: 1,
        }]
    );
    assert_eq!(
        s.agent_name(),
        "build",
        "nothing changes until the server says so"
    );
    let mut script = Script::new();
    script.raw(
        "model.changed",
        json!({"new":{"agent":"plan","model":"a","variant":"high","source":"config","variant_source":"override"},"previous":null,"window":1000,"reason":"agent"}),
    );
    feed(&mut s, &script.out);
    assert_eq!(s.agent_name(), "plan");
    // a refusal arrives as a failed command.result and leaves the agent as it was
    script.raw(
        "command.result",
        json!({"operation_id": Ulid(3).to_string(), "name": "agent", "ok": false,
               "error": "the agent is working: wait for the turn to end or stop it, then switch"}),
    );
    feed(&mut s, &script.out);
    assert_eq!(s.agent_name(), "plan");
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Command { ok: false, text, .. } if text.starts_with("the agent is working")
    ));
    // the next toggle asks for build
    let fx = handle_action(&mut s, &mut ui, Action::ToggleAgent);
    assert!(matches!(
        &fx[0],
        Effect::RunCommand { request, .. } if request.arguments == "build"
    ));
}

#[test]
fn the_session_summary_and_a_background_change_set_each_tabs_agent() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let first = s.generation.clone();
    let meta = ClientEvent::Metadata {
        generation: first.clone(),
        result: Ok(SessionMetadata {
            bypass: false,
            bypass_seq: 0,
            incarnation: None,
            source: "config".into(),
            variant_source: "model_default".into(),
            read_only: false,
            resume_disabled_reason: None,
            session_id: "S1".into(),
            actor_state: "idle".into(),
            workspace: "/ws".into(),
            model: "a".into(),
            agent: Some("plan".into()),
            variant: Some("high".into()),
            parent_session_id: None,
            promptable: true,
            child_status: None,
            current_seq: 0,
        }),
    };
    update(&mut s, meta).unwrap();
    assert_eq!(s.agent_name(), "plan");
    handle_action(&mut s, &mut ui, Action::NewConversation);
    assert_eq!(s.agent_name(), "build", "a new tab starts as build");
    let mut script = Script::new();
    script.raw(
        "model.changed",
        json!({"new":{"agent":"build","model":"a","variant":null,"source":"config","variant_source":"model_default"},"previous":null,"window":1000,"reason":"agent"}),
    );
    update(
        &mut s,
        ClientEvent::Envelope {
            generation: first,
            envelope: script.out[0].clone(),
        },
    )
    .unwrap();
    assert_eq!(s.tabs[0].agent_name(), "build");
}

// ---------------------------------------------------------------- model and variant pick

fn variant_catalog() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            api: withnav::tui::protocol::ApiKind::Chat,
            gateway: "gateway".into(),
            provider_group: "gateway".into(),
            name: "opus".into(),
            upstream_id: "vendor/opus".into(),
            context_window: Some(1_000_000),
            variants: vec!["low".into(), "high".into()],
            default_variant: Some("high".into()),
            ..Default::default()
        },
        ModelEntry {
            api: withnav::tui::protocol::ApiKind::Chat,
            gateway: "gateway".into(),
            provider_group: "gateway".into(),
            name: "flash".into(),
            upstream_id: "vendor/flash".into(),
            context_window: Some(100_000),
            ..Default::default()
        },
    ]
}

fn with_variants() -> State {
    let mut s = st();
    s.catalog = variant_catalog();
    s
}

#[test]
fn the_variant_picked_with_a_model_is_sent_when_the_conversation_starts() {
    let (mut s, mut ui) = (with_variants(), ui());
    ui.variant_pick = Some(VariantPick::NewConversation("opus".into()));
    handle_action(&mut s, &mut ui, Action::PickVariant("low".into()));
    assert_eq!(ui.variant_pick, None);
    assert_eq!(s.selected_model_id.as_deref(), Some("opus"));
    assert_eq!(
        s.variant.as_deref(),
        Some("low"),
        "shown before the conversation exists"
    );
    let fx = handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
    let Effect::Create(request) = &fx[0] else {
        panic!("{fx:?}")
    };
    assert_eq!(request.variant, Some(Some("low".into())));
    assert_eq!(
        serde_json::to_value(request).unwrap()["variant"],
        json!("low")
    );
    // `off` asks for none: an explicit null
    let (mut s, mut ui) = (with_variants(), self::ui());
    ui.variant_pick = Some(VariantPick::NewConversation("opus".into()));
    handle_action(&mut s, &mut ui, Action::PickVariant("off".into()));
    assert_eq!(s.variant, None);
    let fx = handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
    let Effect::Create(request) = &fx[0] else {
        panic!("{fx:?}")
    };
    let body = serde_json::to_value(request).unwrap();
    assert!(body.get("variant").is_some_and(Value::is_null), "{body}");
    // a model picked without the step leaves the server's default: no field
    let (mut s, mut ui) = (with_variants(), self::ui());
    handle_action(&mut s, &mut ui, Action::PickModel("opus".into()));
    assert_eq!(s.variant.as_deref(), Some("high"), "the default is shown");
    let fx = handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
    let Effect::Create(request) = &fx[0] else {
        panic!("{fx:?}")
    };
    assert!(serde_json::to_value(request)
        .unwrap()
        .get("variant")
        .is_none());
}

#[test]
fn picking_a_model_in_a_conversation_still_opens_a_new_tab() {
    let (mut s, mut ui) = (with_variants(), ui());
    s.selected_model_id = Some("flash".into());
    created_now(&mut s, "S1").unwrap();
    ui.variant_pick = Some(VariantPick::NewConversation("opus".into()));
    handle_action(&mut s, &mut ui, Action::PickVariant("high".into()));
    assert_eq!((s.tabs.len(), s.active), (2, 1));
    assert_eq!(s.selected_model_id.as_deref(), Some("opus"));
    assert_eq!(s.tabs[0].selected_model_id.as_deref(), Some("flash"));
}

#[test]
fn slash_variant_lists_the_conversations_variants_and_applies_through_the_server() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use withnav::tui::input::{route_key, Editor};
    let (mut s, mut ui) = (with_variants(), ui());
    s.selected_model_id = Some("opus".into());
    created_now(&mut s, "S1").unwrap();
    // the tab last heard the conversation runs on `high`
    let summary = ClientEvent::Metadata {
        generation: s.generation.clone(),
        result: Ok(SessionMetadata {
            bypass: false,
            bypass_seq: 0,
            incarnation: None,
            source: "config".into(),
            variant_source: "model_default".into(),
            read_only: false,
            resume_disabled_reason: None,
            session_id: "S1".into(),
            actor_state: "idle".into(),
            workspace: "/ws".into(),
            model: "opus".into(),
            agent: None,
            variant: Some("high".into()),
            parent_session_id: None,
            promptable: true,
            child_status: None,
            current_seq: 0,
        }),
    };
    update(&mut s, summary).unwrap();
    assert_eq!(s.variant.as_deref(), Some("high"));
    let mut editor = Editor::new();
    let press = |code, s: &mut State, ui: &mut UiState, editor: &mut Editor| {
        route_key(KeyEvent::new(code, KeyModifiers::NONE), s, ui, editor)
            .map(|a| handle_action(s, ui, a))
    };
    // from the palette: ctrl+p, type "variant", enter
    let fx = handle_action(&mut s, &mut ui, Action::OpenOverlay(Overlay::Commands));
    assert!(fx.is_empty());
    for c in "variant".chars() {
        press(KeyCode::Char(c), &mut s, &mut ui, &mut editor);
    }
    let fx = press(KeyCode::Enter, &mut s, &mut ui, &mut editor).unwrap();
    // the list waits for the server: a bare `/variant` says what this conversation offers
    assert_eq!(
        fx,
        vec![Effect::RunCommand {
            session_id: "S1".into(),
            request: CommandRequest {
                name: "variant".into(),
                arguments: String::new(),
            },
            revision: None,
            token: 1,
        }]
    );
    assert_eq!(ui.overlay, None);
    // the HTTP answer comes before its `command.result` on the stream: the server offers `low`
    // and `max` (not the catalog's `low` and `high`) and runs on `max`
    let answer = ClientEvent::CommandRan {
        generation: s.generation.clone(),
        name: "variant".into(),
        revision: None,
        token: 1,
        result: Ok(CommandResponse {
            operation_id: "op".into(),
            ok: true,
            data: Some(json!({"variant": "max", "offered": ["low", "max"]})),
            error: None,
        }),
    };
    let fx = update(&mut s, answer).unwrap();
    assert_eq!(
        fx,
        vec![Effect::ShowVariants {
            current: Some("max".into())
        }]
    );
    // what the runtime does with that effect
    let Effect::ShowVariants { current } = &fx[0] else {
        unreachable!()
    };
    withnav::tui::reducer::show_variants(&s, &mut ui, current.as_deref());
    assert_eq!(ui.overlay, Some(Overlay::Variants));
    assert_eq!(ui.variant_pick, Some(VariantPick::Current));
    let (rows, _) = withnav::tui::commands::variant_choices(&s, &VariantPick::Current);
    assert_eq!(rows, ["low", "max", "off", "inherit"]);
    assert_eq!(
        rows[ui.selection["Variants"]], "max",
        "starts on the variant the answer names, not the tab's older `high`"
    );
    press(KeyCode::Down, &mut s, &mut ui, &mut editor);
    let fx = press(KeyCode::Enter, &mut s, &mut ui, &mut editor).unwrap();
    assert_eq!(
        fx,
        vec![Effect::RunCommand {
            session_id: "S1".into(),
            request: CommandRequest {
                name: "variant".into(),
                arguments: "off".into(),
            },
            revision: None,
            token: 2,
        }]
    );
    assert_eq!(ui.overlay, None);
    // the server's answer sets what the status line shows
    let mut script = Script::new();
    script.seq = 100;
    script.raw(
        "command.result",
        json!({"operation_id": Ulid(4).to_string(), "name": "variant", "ok": true,
               "data": {"agent":"build","model":"opus","variant_source":"override","variant": null, "offered": ["low", "high"]}}),
    );
    script.raw(
        "command.result",
        json!({"operation_id": Ulid(5).to_string(), "name": "variant", "ok": true,
               "data": {"agent":"build","model":"opus","variant_source":"override","variant": "low", "offered": ["low", "high"], "applies": "next_turn"}}),
    );
    let ev = envelope(&s, &script.out[0]);
    update(&mut s, ev).unwrap();
    assert_eq!(s.variant, None);
    let ev = envelope(&s, &script.out[1]);
    update(&mut s, ev).unwrap();
    assert_eq!(s.variant.as_deref(), Some("low"));
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Command { text, .. } if text.contains("applies next turn")
    ));
}

#[test]
fn slash_variant_without_a_conversation_asks_the_server_nothing() {
    let (mut s, mut ui) = (with_variants(), ui());
    let fx = handle_action(&mut s, &mut ui, Action::OpenOverlay(Overlay::Variants));
    assert!(fx.is_empty());
    assert_eq!(ui.overlay, Some(Overlay::Variants));
}

#[test]
fn a_summary_read_before_a_confirmed_change_does_not_undo_it() {
    let (mut s, _) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let summary = |s: &State, seq: u64, agent: &str, variant: Option<&str>| ClientEvent::Metadata {
        generation: s.generation.clone(),
        result: Ok(SessionMetadata {
            bypass: false,
            bypass_seq: 0,
            incarnation: None,
            source: "config".into(),
            variant_source: "model_default".into(),
            read_only: false,
            resume_disabled_reason: None,
            session_id: "S1".into(),
            actor_state: "idle".into(),
            workspace: "/ws".into(),
            model: "a".into(),
            agent: Some(agent.into()),
            variant: variant.map(str::to_string),
            parent_session_id: None,
            promptable: true,
            child_status: None,
            current_seq: seq,
        }),
    };
    let mut script = Script::new();
    script.seq = 10;
    script.raw(
        "model.changed",
        json!({"new":{"agent":"plan","model":"a","variant":"high","source":"config","variant_source":"override"},"previous":null,"window":1000,"reason":"agent"}),
    );
    script.raw(
        "command.result",
        json!({"operation_id": Ulid(1).to_string(), "name": "variant", "ok": true,
               "data": {"agent":"plan","model":"a","variant_source":"override","variant": "high", "offered": ["low", "high"]}}),
    );
    feed(&mut s, &script.out);
    let (agent_seq, variant_seq) = (script.out[0].seq, script.out[1].seq);
    assert_eq!(
        (s.agent_name(), s.variant.as_deref()),
        ("plan", Some("high"))
    );
    // a summary fetched before both, answered after them
    let stale = summary(&s, agent_seq - 1, "build", None);
    update(&mut s, stale).unwrap();
    assert_eq!(
        (s.agent_name(), s.variant.as_deref()),
        ("plan", Some("high"))
    );
    // one read between the two: the agent is current, the variant is not
    let between = summary(&s, agent_seq, "plan", Some("low"));
    update(&mut s, between).unwrap();
    assert_eq!(
        (s.agent_name(), s.variant.as_deref()),
        ("plan", Some("high"))
    );
    // one read after both is applied
    let fresh = summary(&s, variant_seq, "build", Some("low"));
    update(&mut s, fresh).unwrap();
    assert_eq!(
        (s.agent_name(), s.variant.as_deref()),
        ("build", Some("low"))
    );
}

#[test]
fn local_routing_follows_the_server_prefix_rule_and_checks_arguments() {
    use withnav::tui::commands::{route, Routed};
    let mode = Mode::NextTurn;
    // only a slash in the first column makes a command
    assert_eq!(route("  /stop", mode), Routed::Message("  /stop".into()));
    assert_eq!(
        route("\t/context", mode),
        Routed::Message("\t/context".into())
    );
    // the escape keeps everything after it, whitespace included
    assert_eq!(
        route("//  spaced ", mode),
        Routed::Message("/  spaced ".into())
    );
    // client commands that take no argument refuse one
    assert_eq!(route("/stop", mode), Routed::Local(Action::Stop));
    assert_eq!(
        route("/stop now please", mode),
        Routed::Refused("/stop takes no arguments".into())
    );
    assert_eq!(
        route("/quit later", mode),
        Routed::Refused("/quit takes no arguments".into())
    );
    assert!(matches!(route("/sessions x", mode), Routed::Refused(_)));
    // `/model <alias>` switches this conversation; `/new <alias>` opens a tab; one alias only
    assert_eq!(
        route("/model", mode),
        Routed::Local(Action::OpenOverlay(Overlay::Models))
    );
    assert_eq!(
        route("/model opus", mode),
        Routed::Local(Action::SwitchModel("opus".into()))
    );
    assert_eq!(
        route("/new opus", mode),
        Routed::Local(Action::PickAgent("opus".into()))
    );
    assert_eq!(
        route("/model opus high", mode),
        Routed::Refused("usage: /model [agent or model]".into())
    );
}

#[test]
fn a_refused_local_command_shows_why_and_sends_nothing() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let fx = handle_action(
        &mut s,
        &mut ui,
        Action::ShowNotice("/stop takes no arguments".into()),
    );
    assert!(fx.is_empty());
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Notice(n) if n == "/stop takes no arguments"
    ));
}

#[test]
fn slash_variant_always_offers_off_even_with_an_empty_catalog() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    s.variant = Some("high".into());
    assert!(s.catalog.is_empty());
    // before any answer, and with an offer of nothing: `off` is still there
    assert_eq!(
        withnav::tui::commands::variant_choices(&s, &VariantPick::Current),
        (vec!["off".to_string(), "inherit".to_string()], 0)
    );
    let answer = ClientEvent::CommandRan {
        generation: s.generation.clone(),
        name: "variant".into(),
        revision: None,
        token: 1,
        result: Ok(CommandResponse {
            operation_id: "op".into(),
            ok: true,
            data: Some(json!({"variant": "high", "offered": ["high"]})),
            error: None,
        }),
    };
    handle_action(&mut s, &mut ui, Action::OpenOverlay(Overlay::Variants));
    update(&mut s, answer).unwrap();
    let (rows, start) = withnav::tui::commands::variant_choices(&s, &VariantPick::Current);
    assert_eq!(
        (rows, start),
        (vec!["high".into(), "off".into(), "inherit".into()], 0)
    );
    // a failed answer opens nothing
    handle_action(&mut s, &mut ui, Action::OpenOverlay(Overlay::Variants));
    let failed = ClientEvent::CommandRan {
        generation: s.generation.clone(),
        name: "variant".into(),
        revision: None,
        token: 1,
        result: Err("HTTP 503".into()),
    };
    assert!(update(&mut s, failed).unwrap().is_empty());
    assert!(!s.variant_list_wanted);
}

#[test]
fn picking_a_model_while_a_conversation_is_being_created_opens_another_tab() {
    let (mut s, mut ui) = (with_variants(), ui());
    handle_action(&mut s, &mut ui, Action::PickModel("flash".into()));
    let fx = handle_action(&mut s, &mut ui, Action::Submit("hi".into()));
    assert!(matches!(&fx[0], Effect::Create(r) if r.model.as_deref() == Some("flash")));
    let creating = s.generation.clone();
    // before the server answers, another model is picked
    handle_action(&mut s, &mut ui, Action::PickModel("opus".into()));
    assert_eq!((s.tabs.len(), s.active), (2, 1));
    assert_eq!(s.selected_model_id.as_deref(), Some("opus"));
    assert_eq!(s.tabs[0].selected_model_id.as_deref(), Some("flash"));
    // the creation lands on its own tab, with its own model
    update(
        &mut s,
        ClientEvent::Created {
            generation: creating,
            result: Ok(CreateSessionResponse {
                session_id: "S1".into(),
                root_actor_id: "agent:main".into(),
                actor_state: "idle".into(),
            }),
        },
    )
    .unwrap();
    assert_eq!(s.tabs[0].session.as_ref().unwrap().model, "flash");
    // the same while a resume is in flight
    let (mut s, mut ui) = (with_variants(), self::ui());
    handle_action(&mut s, &mut ui, Action::ResumeSession("S9".into()));
    handle_action(&mut s, &mut ui, Action::PickModel("opus".into()));
    assert_eq!((s.tabs.len(), s.active), (2, 1));
    assert_eq!(s.tabs[0].resuming.as_deref(), Some("S9"));
}

// ---------------------------------------------------------------- permission requests

/// A turn whose `write` call waits for a decision: llm.response with the call, then
/// permission.requested and the yielded turn end, built from the server's own types.
fn permission_script(request: u128) -> Script {
    let mut s = Script::new();
    s.start().user("write a.txt").turn_start(1);
    s.response(
        1,
        1,
        "",
        "",
        vec![call(
            "w1",
            "write",
            json!({"path": "/ws/a.txt", "content": "one\ntwo"}),
        )],
        Some(usage(10, 2)),
    );
    s.push(
        EventKind::PermissionRequested,
        &PermissionRequested {
            child_session_id: None,
            request_id: Ulid(request),
            tool_call_id: "w1".into(),
            tool: "write".into(),
            arguments: json!({"path": "/ws/a.txt", "content": "one\ntwo"}).to_string(),
            scope: withnav::agent::permissions::Scope::Folder("/ws".into()),
            agent: "build".into(),
            reason: "write changes a file".into(),
        },
    );
    s.turn_end(1, TurnEndOutcome::Yielded, None);
    s
}

fn resolved(s: &mut Script, request: u128, resolution: PermissionResolution) {
    s.push(
        EventKind::PermissionResolved,
        &PermissionResolved {
            child_session_id: None,
            request_id: Ulid(request),
            tool_call_id: "w1".into(),
            resolution,
        },
    );
}

#[test]
fn a_permission_request_is_open_until_resolved_live_and_on_replay() {
    let script = permission_script(77);
    let mut s = st();
    feed(&mut s, &script.out);
    let open = s.open_permission().expect("an open request");
    assert_eq!(open.request_id, Ulid(77).to_string());
    assert_eq!(
        (
            open.tool.as_str(),
            open.scope_kind.as_str(),
            open.scope.as_str()
        ),
        ("write", "folder", "/ws")
    );
    assert_eq!(s.activity, Activity::AwaitingUser);
    assert_eq!(
        tool(&s, "w1").status,
        None,
        "the held call is not settled by the yield"
    );
    // replay from the start gives the same open request
    let mut again = st();
    feed(&mut again, &script.out);
    assert_eq!(again.conversation.blocks, s.conversation.blocks);
    assert_eq!(again.open_permission(), s.open_permission());
    // the same event twice adds nothing
    let mut twice = st();
    let mut dup = script.out.clone();
    let mut copy = dup[dup.len() - 2].clone();
    copy.seq = 999;
    dup.push(copy);
    feed(&mut twice, &dup);
    assert_eq!(twice.conversation.open_permissions.len(), 1);

    // resolved: no longer open, and the block says how
    let mut script = permission_script(77);
    resolved(&mut script, 77, PermissionResolution::AllowOnce);
    let mut s = st();
    feed(&mut s, &script.out);
    assert!(s.open_permission().is_none());
    assert!(s.conversation.blocks.iter().any(|b| matches!(
        &b.kind,
        BlockKind::Permission(p) if p.resolution.as_deref() == Some("allow_once")
    )));
}

#[test]
fn a_denial_or_a_cancel_settles_the_held_call() {
    let mut script = permission_script(5);
    resolved(
        &mut script,
        5,
        PermissionResolution::Deny {
            message: "not that file".into(),
        },
    );
    let mut s = st();
    feed(&mut s, &script.out);
    assert_eq!(tool(&s, "w1").status, Some(ToolStatus::Denied));
    assert!(s.conversation.blocks.iter().any(|b| matches!(
        &b.kind,
        BlockKind::Permission(p) if p.resolution.as_deref() == Some("deny")
            && p.message.as_deref() == Some("not that file")
    )));
    let mut script = permission_script(6);
    resolved(&mut script, 6, PermissionResolution::Cancelled);
    let mut s = st();
    feed(&mut s, &script.out);
    assert_eq!(tool(&s, "w1").status, Some(ToolStatus::Interrupted));
    assert!(s.open_permission().is_none());
}

#[test]
fn a_fresh_request_after_a_retarget_replaces_the_prompt_for_that_call() {
    // allowed, but the call now lands elsewhere: the server asks again for w1
    let mut script = permission_script(10);
    resolved(&mut script, 10, PermissionResolution::AllowOnce);
    script.turn_start(2);
    script.push(
        EventKind::PermissionRequested,
        &PermissionRequested {
            child_session_id: None,
            request_id: Ulid(11),
            tool_call_id: "w1".into(),
            tool: "write".into(),
            arguments: json!({"path": "/elsewhere/a.txt", "content": "one\ntwo"}).to_string(),
            scope: withnav::agent::permissions::Scope::Folder("/elsewhere".into()),
            agent: "build".into(),
            reason: "write changes a file".into(),
        },
    );
    script.turn_end(2, TurnEndOutcome::Yielded, None);
    let mut s = st();
    feed(&mut s, &script.out);
    let prompts: Vec<_> = s
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Permission(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(prompts.len(), 1, "one prompt for w1, not two");
    assert_eq!(prompts[0].request_id, Ulid(11).to_string());
    assert_eq!(prompts[0].scope, "/elsewhere");
    assert_eq!(prompts[0].resolution, None);
    assert_eq!(
        s.open_permission().map(|p| p.request_id.clone()),
        Some(Ulid(11).to_string())
    );
    assert_eq!(s.conversation.open_permissions.len(), 1);
    // replay gives the same single prompt
    let mut again = st();
    feed(&mut again, &script.out);
    assert_eq!(again.conversation.blocks, s.conversation.blocks);

    // a denied call asked again later (a new turn reusing the id) keeps its record
    let mut script = permission_script(20);
    resolved(
        &mut script,
        20,
        PermissionResolution::Deny {
            message: "no".into(),
        },
    );
    script.push(
        EventKind::PermissionRequested,
        &PermissionRequested {
            child_session_id: None,
            request_id: Ulid(21),
            tool_call_id: "w1".into(),
            tool: "write".into(),
            arguments: "{}".into(),
            scope: withnav::agent::permissions::Scope::Folder("/ws".into()),
            agent: "build".into(),
            reason: String::new(),
        },
    );
    let mut s = st();
    feed(&mut s, &script.out);
    let count = s
        .conversation
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Permission(_)))
        .count();
    assert_eq!(count, 2);
}

#[test]
fn a_background_tab_with_an_open_request_waits_and_is_unread() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let first = s.generation.clone();
    handle_action(&mut s, &mut ui, Action::NewConversation);
    for e in permission_script(9).out {
        update(
            &mut s,
            ClientEvent::Envelope {
                generation: first.clone(),
                envelope: e,
            },
        )
        .unwrap();
    }
    let background = &s.tabs[0];
    assert!(background.open_permission().is_some());
    assert!(background.unread);
    assert_eq!(
        withnav::tui::view::sidebar::tab_state(background),
        "waiting"
    );
    assert!(s.open_permission().is_none(), "the tab on screen has none");
}

fn answer_effects(s: &mut State, ui: &mut UiState, request: u128, decision: &str) -> Vec<Effect> {
    handle_action(
        s,
        ui,
        Action::AnswerPermission {
            request_id: Ulid(request).to_string(),
            answer: PermissionAnswer {
                decision: decision.into(),
                message: None,
            },
        },
    )
}

#[test]
fn an_accepted_answer_waiting_for_its_resolution_is_not_sent_again() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    feed(&mut s, &permission_script(30).out);
    assert_eq!(answer_effects(&mut s, &mut ui, 30, "allow_once").len(), 1);
    // the server took it; permission.resolved is late
    let ok = ClientEvent::PermissionAnswered {
        generation: s.generation.clone(),
        request_id: Ulid(30).to_string(),
        result: Ok(()),
    };
    update(&mut s, ok).unwrap();
    for decision in ["allow_once", "allow_folder", "deny"] {
        assert!(answer_effects(&mut s, &mut ui, 30, decision).is_empty());
    }
    // the resolution ends it; a fresh request is answerable at once
    let mut script = permission_script(30);
    resolved(&mut script, 30, PermissionResolution::AllowOnce);
    script.turn_start(2);
    script.push(
        EventKind::PermissionRequested,
        &PermissionRequested {
            child_session_id: None,
            request_id: Ulid(31),
            tool_call_id: "w1".into(),
            tool: "write".into(),
            arguments: "{}".into(),
            scope: withnav::agent::permissions::Scope::Folder("/other".into()),
            agent: "build".into(),
            reason: String::new(),
        },
    );
    let skip = permission_script(30).out.len();
    feed(&mut s, &script.out[skip..]);
    assert!(s.answered_permission.is_none());
    assert_eq!(answer_effects(&mut s, &mut ui, 31, "allow_once").len(), 1);
}

#[test]
fn a_resolution_before_the_http_reply_leaves_nothing_answered() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let mut script = permission_script(40);
    let skip = script.out.len();
    resolved(&mut script, 40, PermissionResolution::AllowOnce);
    feed(&mut s, &script.out[..skip]);
    assert_eq!(answer_effects(&mut s, &mut ui, 40, "allow_once").len(), 1);
    feed(&mut s, &script.out[skip..]);
    assert!(s.open_permission().is_none());
    let ok = ClientEvent::PermissionAnswered {
        generation: s.generation.clone(),
        request_id: Ulid(40).to_string(),
        result: Ok(()),
    };
    update(&mut s, ok).unwrap();
    assert!(s.answered_permission.is_none());
    assert!(answer_effects(&mut s, &mut ui, 40, "allow_once").is_empty());
    assert!(s.conversation.blocks.iter().any(|b| matches!(
        &b.kind,
        BlockKind::Permission(p) if p.resolution.as_deref() == Some("allow_once")
    )));
}

#[test]
fn answering_posts_once_and_a_stale_answer_drops_the_prompt_with_a_notice() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    feed(&mut s, &permission_script(3).out);
    let request_id = Ulid(3).to_string();
    let answer = || Action::AnswerPermission {
        request_id: request_id.clone(),
        answer: PermissionAnswer {
            decision: "allow_once".into(),
            message: None,
        },
    };
    let fx = handle_action(&mut s, &mut ui, answer());
    assert_eq!(
        fx,
        vec![Effect::AnswerPermission {
            session_id: "S1".into(),
            request_id: request_id.clone(),
            answer: PermissionAnswer {
                decision: "allow_once".into(),
                message: None,
            },
        }]
    );
    assert!(
        handle_action(&mut s, &mut ui, answer()).is_empty(),
        "one answer in flight at a time"
    );
    // accepted: the prompt stays until permission.resolved arrives
    let ok = ClientEvent::PermissionAnswered {
        generation: s.generation.clone(),
        request_id: request_id.clone(),
        result: Ok(()),
    };
    update(&mut s, ok).unwrap();
    assert!(s.open_permission().is_some());
    assert!(
        handle_action(&mut s, &mut ui, answer()).is_empty(),
        "still answered until the server ends it"
    );
    // stale: dropped with a short notice
    let stale = ClientEvent::PermissionAnswered {
        generation: s.generation.clone(),
        request_id: request_id.clone(),
        result: Err(AnswerError::Stale),
    };
    update(&mut s, stale).unwrap();
    assert!(s.open_permission().is_none());
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Notice(n) if n.contains("no longer waiting")
    ));
    let prompt = |s: &State| {
        s.conversation
            .blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Permission(p) => Some(p.clone()),
                _ => None,
            })
            .unwrap()
    };
    // marked locally, not given a decision the server did not send
    let marked = prompt(&s);
    assert!(marked.stale && !marked.is_open());
    assert_eq!(marked.resolution, None);
    assert!(handle_action(&mut s, &mut ui, answer()).is_empty());
    // the server's resolution, arriving later, says how it ended
    let mut script = permission_script(3);
    let skip = script.out.len();
    resolved(&mut script, 3, PermissionResolution::Cancelled);
    feed(&mut s, &script.out[skip..]);
    assert_eq!(prompt(&s).resolution.as_deref(), Some("cancelled"));
    assert_eq!(tool(&s, "w1").status, Some(ToolStatus::Interrupted));
    // an answer for another request than the open one sends nothing
    let mut other = st();
    created_now(&mut other, "S2").unwrap();
    feed(&mut other, &permission_script(4).out);
    assert!(handle_action(&mut other, &mut ui, answer()).is_empty());
}

// ---------------------------------------------------------------- git state

fn git_now(s: &mut State, branch: Option<&str>) -> Vec<Effect> {
    let ev = ClientEvent::Git {
        generation: s.generation.clone(),
        status: branch.map(|b| withnav::workspace::git::GitStatus {
            branch: Some(b.into()),
            dirty: 2,
            files: vec![],
        }),
    };
    update(s, ev).unwrap()
}

#[test]
fn the_git_state_is_read_at_start_and_after_each_turn_one_read_at_a_time() {
    let mut s = st();
    let fx = created_now(&mut s, "S1").unwrap();
    assert_eq!(fx.iter().filter(|e| **e == Effect::ReadGit).count(), 1);
    // a turn ends while the first read runs: one more read once it returns, not two at once
    let mut script = Script::new();
    script.start().user("hi").turn_start(1);
    script.turn_end(1, TurnEndOutcome::Done, Some(usage(1, 1)));
    let mut asked = 0;
    for e in &script.out {
        let ev = envelope(&s, e);
        asked += update(&mut s, ev)
            .unwrap()
            .iter()
            .filter(|e| **e == Effect::ReadGit)
            .count();
    }
    assert_eq!(asked, 0, "a read is already running");
    assert_eq!(git_now(&mut s, Some("main")), vec![Effect::ReadGit]);
    assert_eq!(s.git.as_ref().unwrap().branch.as_deref(), Some("main"));
    assert!(git_now(&mut s, Some("main")).is_empty());
    // not a repository: nothing shown
    let mut again = script.out.last().unwrap().clone();
    again.seq += 100;
    let ev = envelope(&s, &again);
    assert_eq!(update(&mut s, ev).unwrap(), vec![Effect::ReadGit]);
    git_now(&mut s, None);
    assert_eq!(s.git, None);
}

#[test]
fn a_git_result_reaches_the_tab_that_asked_and_a_stale_one_is_dropped() {
    let (mut s, mut ui) = (st(), ui());
    created_now(&mut s, "S1").unwrap();
    let first = s.generation.clone();
    handle_action(&mut s, &mut ui, Action::NewConversation);
    assert_eq!(s.active, 1);
    let status = |branch: &str| withnav::workspace::git::GitStatus {
        branch: Some(branch.into()),
        dirty: 1,
        files: vec![],
    };
    // the first tab, now in the background, gets its own result
    update(
        &mut s,
        ClientEvent::Git {
            generation: first.clone(),
            status: Some(status("one")),
        },
    )
    .unwrap();
    assert_eq!(s.tabs[0].git, Some(status("one")));
    assert!(!s.tabs[0].git_reading);
    assert_eq!(s.git, None, "the tab on screen is untouched");
    // a result for a generation no tab holds (the tab was closed or moved on) changes nothing
    let before = s.clone();
    update(
        &mut s,
        ClientEvent::Git {
            generation: Generation(9_999),
            status: Some(status("gone")),
        },
    )
    .unwrap();
    assert_eq!(s, before);
}

// ---------------------------------------------------------------- model switch

fn model_command(alias: &str) -> Effect {
    model_command_at(alias, 1)
}

/// The `/model <alias>` request the tab sends as its `token`th command.
fn model_command_at(alias: &str, token: u64) -> Effect {
    Effect::RunCommand {
        session_id: "S1".into(),
        request: CommandRequest {
            name: "model".into(),
            arguments: alias.into(),
        },
        revision: None,
        token,
    }
}

#[test]
fn switching_a_live_conversation_sends_model_and_opens_no_tab() {
    let (mut s, mut ui) = (st(), ui());
    models_now(&mut s, Some("a".into())).unwrap();
    created_now(&mut s, "S1").unwrap();
    let fx = handle_action(&mut s, &mut ui, Action::SwitchModel("b".into()));
    assert_eq!(fx, vec![model_command("b")]);
    assert_eq!(s.tabs.len(), 1, "no new tab");
    assert_eq!(
        s.model_switch.as_ref().map(|m| m.model_id.as_str()),
        Some("b")
    );
    assert_eq!(
        s.selected_model_id.as_deref(),
        Some("a"),
        "the committed model stays until the server says otherwise"
    );
    // an empty tab starts on the model instead
    let mut fresh = st();
    models_now(&mut fresh, None).unwrap();
    assert!(handle_action(&mut fresh, &mut ui, Action::SwitchModel("b".into())).is_empty());
    assert_eq!(fresh.selected_model_id.as_deref(), Some("b"));
    // `/new <alias>` (PickModel) still opens a tab
    handle_action(&mut s, &mut ui, Action::PickModel("b".into()));
    assert_eq!((s.tabs.len(), s.active), (2, 1));
}

#[test]
fn no_switch_is_sent_while_the_agent_waits_for_the_captain() {
    let (mut s, mut ui) = (st(), ui());
    models_now(&mut s, Some("a".into())).unwrap();
    created_now(&mut s, "S1").unwrap();
    feed(&mut s, &permission_script(5).out);
    assert!(handle_action(&mut s, &mut ui, Action::SwitchModel("b".into())).is_empty());
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Command { ok: false, text, .. } if text.contains("waiting for approval")
    ));
    assert_eq!(s.model_switch, None);
    // a question
    let mut s = st();
    models_now(&mut s, Some("a".into())).unwrap();
    created_now(&mut s, "S1").unwrap();
    s.activity = Activity::AwaitingUser;
    assert!(handle_action(&mut s, &mut ui, Action::SwitchModel("b".into())).is_empty());
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Command { ok: false, text, .. } if text.contains("waiting for your answer")
    ));
}

fn model_changed_op(
    s: &mut Script,
    op: Option<u128>,
    from: Option<&str>,
    to: &str,
    variant: Option<&str>,
    window: u64,
) {
    let identity = |model: &str| ModelIdentity {
        agent: "build".into(),
        selection: Selection {
            model: model.into(),
            variant: variant.map(str::to_string),
            source: "override".into(),
            variant_source: "override".into(),
        },
    };
    s.push(EventKind::ModelChanged, &serde_json::json!({
        "operation_id": op.map(Ulid), "reason": if from.is_some() { "model" } else { "initial" },
        "previous": from.map(identity), "new": identity(to), "window": window
    }));
}

fn model_result(s: &mut Script, op: u128, ok: bool, data: Option<Value>, error: Option<&str>) {
    s.push(
        EventKind::CommandResult,
        &CommandResult {
            operation_id: Ulid(op),
            name: "model".into(),
            ok,
            data,
            error: error.map(str::to_string),
        },
    );
}

fn deferred() -> Option<Value> {
    Some(json!({"accepted": true, "applies": "after_turn"}))
}

/// The HTTP answer to the tab's latest `/model`, carrying operation `op`.
fn model_answered(s: &mut State, op: u128, result: Result<Option<Value>, &str>) {
    let token = s.model_switch.as_ref().map_or(0, |m| m.token);
    model_answered_to(s, token, op, result);
}

/// The HTTP answer to the tab's `/model` request `token`, carrying operation `op`.
fn model_answered_to(s: &mut State, token: u64, op: u128, result: Result<Option<Value>, &str>) {
    let ev = ClientEvent::CommandRan {
        generation: s.generation.clone(),
        name: "model".into(),
        revision: None,
        token,
        result: match result {
            Ok(data) => Ok(CommandResponse {
                operation_id: Ulid(op).to_string(),
                ok: true,
                data,
                error: None,
            }),
            Err(e) => Err(e.into()),
        },
    };
    update(s, ev).unwrap();
}

/// A live conversation on `a`, its turn running, that just asked to switch to `b`.
fn switching() -> (State, UiState) {
    let (mut s, mut ui) = (st(), ui());
    models_now(&mut s, Some("a".into())).unwrap();
    s.catalog.push(ModelEntry {
        api: withnav::tui::protocol::ApiKind::Chat,
        name: "c".into(),
        ..Default::default()
    });
    created_now(&mut s, "S1").unwrap();
    s.activity = Activity::Running;
    assert_eq!(
        handle_action(&mut s, &mut ui, Action::SwitchModel("b".into())),
        vec![model_command("b")]
    );
    (s, ui)
}

/// `script`'s events renumbered after `from`, so they are new to a state that applied `from`.
fn after(script: &mut Script, from: u64) {
    for e in &mut script.out {
        e.seq += from;
    }
}

#[test]
fn model_changed_moves_the_tab_to_the_new_model_live_and_on_replay() {
    let mut script = Script::new();
    script.start();
    model_changed_op(&mut script, None, None, "opus", None, 200_000);
    model_changed_op(
        &mut script,
        Some(77),
        Some("opus"),
        "luna",
        Some("high"),
        872_000,
    );
    model_result(
        &mut script,
        77,
        true,
        Some(json!({"model": "luna", "changed": true, "applies": "now"})),
        None,
    );
    let mut s = st();
    feed(&mut s, &script.out);
    assert_eq!(s.selected_model_id.as_deref(), Some("luna"));
    assert_eq!(s.selected_model_name.as_deref(), Some("luna"));
    assert_eq!(s.variant.as_deref(), Some("high"));
    assert_eq!(s.context_window, Some(872_000));
    let notices: Vec<String> = s
        .conversation
        .blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Notice(n) => Some(n.clone()),
            BlockKind::Command { name, text, .. } if name == "model" => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        ["model: opus → luna · high · 872k"],
        "one compact notice; the start and the done result add none"
    );
    let mut again = st();
    feed(&mut again, &script.out);
    assert_eq!(again.conversation.blocks, s.conversation.blocks);
    assert_eq!(again.selected_model_id, s.selected_model_id);
    // a session summary read before the switch does not undo it
    let old = ClientEvent::Metadata {
        generation: s.generation.clone(),
        result: Ok(SessionMetadata {
            bypass: false,
            bypass_seq: 0,
            incarnation: None,
            source: "config".into(),
            variant_source: "model_default".into(),
            read_only: false,
            resume_disabled_reason: None,
            session_id: "S1".into(),
            actor_state: "idle".into(),
            workspace: "/ws".into(),
            model: "opus".into(),
            agent: None,
            variant: None,
            parent_session_id: None,
            promptable: true,
            child_status: None,
            current_seq: 1,
        }),
    };
    update(&mut s, old).unwrap();
    assert_eq!(s.selected_model_id.as_deref(), Some("luna"));
    assert_eq!(s.variant.as_deref(), Some("high"));
}

#[test]
fn an_accepted_switch_waits_until_the_selection_of_its_own_operation() {
    let (mut s, _) = switching();
    assert_eq!(
        s.pending_model(),
        None,
        "not shown before the server accepted it"
    );
    model_answered(&mut s, 77, Ok(deferred()));
    let mut script = Script::new();
    model_result(&mut script, 77, true, deferred(), None);
    feed(&mut s, &script.out);
    assert_eq!(s.pending_model(), Some("b"));
    assert_eq!(
        s.selected_model_id.as_deref(),
        Some("a"),
        "still the one in use"
    );
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Command { text, .. } if text == "switches to b after this turn"
    ));
    // a model.changed of another operation (another client's switch) does not end it
    let mut other = Script::new();
    model_changed_op(&mut other, Some(99), Some("a"), "c", None, 1000);
    after(&mut other, 50);
    feed(&mut s, &other.out);
    assert_eq!(s.pending_model(), Some("b"));
    // its own selection does
    let mut own = Script::new();
    model_changed_op(&mut own, Some(77), Some("c"), "b", None, 1000);
    after(&mut own, 60);
    feed(&mut s, &own.out);
    assert_eq!(s.pending_model(), None);
    assert_eq!(s.selected_model_id.as_deref(), Some("b"));
}

#[test]
fn the_result_of_its_operation_settles_the_switch_whatever_the_order() {
    // refused after the turn: the result of that operation clears the mark
    let (mut s, _) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    assert_eq!(s.pending_model(), Some("b"));
    let mut script = Script::new();
    model_result(&mut script, 78, false, None, Some("not this one"));
    model_result(
        &mut script,
        77,
        false,
        None,
        Some("the session stopped before the switch"),
    );
    feed(&mut s, &script.out);
    assert_eq!(s.pending_model(), None);
    assert_eq!(s.model_switch, None);
    // the stream's result before the HTTP answer: the answer marks nothing
    let (mut s, _) = switching();
    let mut script = Script::new();
    model_result(&mut script, 77, true, deferred(), None);
    model_changed_op(&mut script, Some(77), Some("a"), "b", None, 1000);
    feed(&mut s, &script.out);
    model_answered(&mut s, 77, Ok(deferred()));
    assert_eq!(s.pending_model(), None);
    assert_eq!(s.selected_model_id.as_deref(), Some("b"));
    // a switch made at once is settled by its answer
    let (mut s, _) = switching();
    model_answered(
        &mut s,
        77,
        Ok(Some(json!({"changed": true, "applies": "now"}))),
    );
    assert_eq!(s.model_switch, None);
}

#[test]
fn a_new_switch_replaces_the_waiting_one() {
    let (mut s, mut ui) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    assert_eq!(
        handle_action(&mut s, &mut ui, Action::SwitchModel("c".into())),
        vec![model_command_at("c", 2)]
    );
    assert_eq!(s.pending_model(), None, "the new one is not accepted yet");
    model_answered(&mut s, 88, Ok(deferred()));
    assert_eq!(s.pending_model(), Some("c"));
    // the old operation settling later does not clear the new one
    let mut script = Script::new();
    model_result(&mut script, 77, false, None, Some("superseded"));
    feed(&mut s, &script.out);
    assert_eq!(s.pending_model(), Some("c"));
}

/// A live conversation on `a`, its turn running, that asked for `b` (request 1) and then `c`
/// (request 2) before either HTTP answer came back.
fn switching_twice() -> (State, UiState) {
    let (mut s, mut ui) = switching();
    assert_eq!(
        handle_action(&mut s, &mut ui, Action::SwitchModel("c".into())),
        vec![model_command_at("c", 2)]
    );
    (s, ui)
}

#[test]
fn only_the_answer_to_the_latest_request_binds_the_waiting_switch() {
    // b's answer first, then c's
    let (mut s, _) = switching_twice();
    model_answered_to(&mut s, 1, 77, Ok(deferred()));
    assert_eq!(
        s.pending_model(),
        None,
        "b's answer does not take c's place"
    );
    assert_eq!(s.model_switch.as_ref().unwrap().operation_id, None);
    model_answered_to(&mut s, 2, 88, Ok(deferred()));
    assert_eq!(s.pending_model(), Some("c"));
    // b settling does not clear c
    let mut script = Script::new();
    model_result(&mut script, 77, false, None, Some("refused"));
    model_changed_op(&mut script, Some(77), Some("a"), "b", None, 1000);
    feed(&mut s, &script.out);
    assert_eq!(s.pending_model(), Some("c"));
    // c's own selection does
    let mut own = Script::new();
    model_changed_op(&mut own, Some(88), Some("b"), "c", None, 1000);
    after(&mut own, 50);
    feed(&mut s, &own.out);
    assert_eq!(s.pending_model(), None);

    // c's answer first, then b's
    let (mut s, _) = switching_twice();
    model_answered_to(&mut s, 2, 88, Ok(deferred()));
    assert_eq!(s.pending_model(), Some("c"));
    model_answered_to(&mut s, 1, 77, Ok(deferred()));
    assert_eq!(s.pending_model(), Some("c"));
    assert_eq!(
        s.model_switch.as_ref().unwrap().operation_id.as_deref(),
        Some(Ulid(88).to_string().as_str()),
        "still c's operation"
    );
}

#[test]
fn an_older_request_failing_leaves_the_newer_switch() {
    let (mut s, _) = switching_twice();
    model_answered_to(&mut s, 1, 0, Err("connection reset"));
    assert_eq!(
        s.model_switch.as_ref().map(|m| m.model_id.as_str()),
        Some("c")
    );
    model_answered_to(&mut s, 2, 88, Ok(deferred()));
    assert_eq!(s.pending_model(), Some("c"));
    // and the newer one failing does end it
    let (mut s, _) = switching_twice();
    model_answered_to(&mut s, 2, 0, Err("connection reset"));
    assert_eq!(s.model_switch, None);
}

#[test]
fn the_switch_mark_ends_with_a_failed_request_a_stop_a_session_end_and_a_closed_tab() {
    // the request never reached the server
    let (mut s, _) = switching();
    model_answered(&mut s, 0, Err("connection refused"));
    assert_eq!(s.model_switch, None);
    // a stop
    let (mut s, _) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    let stopped = ClientEvent::Stopped {
        generation: s.generation.clone(),
        result: Ok(StopResponse {
            accepted: true,
            actor_state: "stopped".into(),
        }),
    };
    update(&mut s, stopped).unwrap();
    assert_eq!(s.model_switch, None);
    // the session ended
    let (mut s, _) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    let mut end = Script::new();
    end.push(EventKind::SessionEnd, &json!({"reason": "stopped"}));
    after(&mut end, 40);
    feed(&mut s, &end.out);
    assert_eq!(s.model_switch, None);
    // the tab was closed: the tab that comes forward shows none
    let (mut s, mut ui) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    handle_action(&mut s, &mut ui, Action::CloseTab);
    assert_eq!(s.model_switch, None);
}

#[test]
fn after_a_reconnect_the_mark_shows_only_if_the_replay_still_reports_it() {
    // the replay holds the acceptance and nothing that settles it: still waiting
    let (mut s, _) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    let resync = ClientEvent::Resync {
        generation: s.generation.clone(),
    };
    update(&mut s, resync.clone()).unwrap();
    assert_eq!(s.pending_model(), None, "hidden until the replay says so");
    let mut replay = Script::new();
    replay.start();
    model_result(&mut replay, 77, true, deferred(), None);
    feed(&mut s, &replay.out);
    assert_eq!(s.pending_model(), Some("b"));
    // the replay also holds its selection: settled
    update(&mut s, resync.clone()).unwrap();
    model_changed_op(&mut replay, Some(77), Some("a"), "b", None, 1000);
    feed(&mut s, &replay.out);
    assert_eq!(s.pending_model(), None);
    assert_eq!(s.model_switch, None);
    // the replay holds no acceptance at all (a restarted server): it stays hidden
    let (mut s, _) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    update(&mut s, resync).unwrap();
    let mut empty = Script::new();
    empty.start();
    feed(&mut s, &empty.out);
    assert_eq!(s.pending_model(), None);
}

#[test]
fn a_refused_switch_shows_the_servers_reason_and_changes_nothing() {
    let (mut s, _) = switching();
    let mut script = Script::new();
    model_result(
        &mut script,
        77,
        false,
        None,
        Some("the conversation does not fit b's window: /compact first"),
    );
    feed(&mut s, &script.out);
    model_answered(&mut s, 77, Ok(None));
    assert_eq!(s.selected_model_id.as_deref(), Some("a"));
    assert_eq!(s.model_switch, None);
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Command { ok: false, text, .. }
            if text == "the conversation does not fit b's window: /compact first"
    ));
}

#[test]
fn events_replays_and_a_resync_never_send_a_command() {
    let (mut s, _) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    let mut script = Script::new();
    model_result(&mut script, 77, true, deferred(), None);
    model_changed_op(&mut script, Some(77), Some("a"), "b", Some("high"), 1000);
    // each event, then the same events again (a duplicate delivery), send nothing
    for e in script.out.iter().chain(script.out.iter()) {
        let ev = envelope(&s, e);
        assert!(
            update(&mut s, ev)
                .unwrap()
                .iter()
                .all(|fx| matches!(fx, Effect::FetchAgents(_))),
            "{}",
            e.kind
        );
    }
    // a resync while a switch is pending re-subscribes, and asks the server for nothing else
    let (mut s, _) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    let resync = ClientEvent::Resync {
        generation: s.generation.clone(),
    };
    let fx = update(&mut s, resync).unwrap();
    assert!(
        fx.iter().all(|e| matches!(e, Effect::Subscribe { .. })),
        "{fx:?}"
    );
    for e in &script.out {
        let ev = envelope(&s, e);
        assert!(update(&mut s, ev)
            .unwrap()
            .iter()
            .all(|fx| matches!(fx, Effect::FetchAgents(_))));
    }
}

#[test]
fn picking_the_model_in_use_sends_nothing_and_says_so() {
    let (mut s, mut ui) = (st(), ui());
    models_now(&mut s, Some("a".into())).unwrap();
    created_now(&mut s, "S1").unwrap();
    assert!(handle_action(&mut s, &mut ui, Action::SwitchModel("a".into())).is_empty());
    assert_eq!(s.model_switch, None);
    assert!(matches!(
        &s.conversation.blocks.last().unwrap().kind,
        BlockKind::Notice(n) if n == "already on a; use /variant to change the variant"
    ));
    // with a switch to b waiting for the turn, going back to a is a real switch
    s.activity = Activity::Running;
    handle_action(&mut s, &mut ui, Action::SwitchModel("b".into()));
    model_answered(&mut s, 77, Ok(deferred()));
    assert_eq!(
        handle_action(&mut s, &mut ui, Action::SwitchModel("a".into())),
        vec![model_command_at("a", 2)]
    );
}

#[test]
fn a_catalog_refresh_leaves_the_committed_model_and_window_alone() {
    let mut script = Script::new();
    script.start();
    model_changed_op(&mut script, None, None, "luna", Some("high"), 872_000);
    let mut s = st();
    feed(&mut s, &script.out);
    assert_eq!(
        (s.selected_model_name.as_deref(), s.context_window),
        (Some("luna"), Some(872_000))
    );
    // the catalog (F2) now says luna maps to another model with another window
    let ev = ClientEvent::Models {
        workspace: "/ws".into(),
        request: 0,
        generation: s.generation.clone(),
        result: Ok(ModelsBody {
            models: vec![ModelEntry {
                api: withnav::tui::protocol::ApiKind::Chat,
                gateway: "gateway".into(),
                provider_group: "gateway".into(),
                name: "luna".into(),
                upstream_id: "elsewhere/other-model".into(),
                context_window: Some(32_000),
                ..Default::default()
            }],
            default_agent: Some("luna".into()),
        }),
    };
    update(&mut s, ev).unwrap();
    assert_eq!(s.catalog.len(), 1, "the picker gets the new list");
    assert_eq!(s.selected_model_id.as_deref(), Some("luna"));
    assert_eq!(
        (s.selected_model_name.as_deref(), s.context_window),
        (Some("luna"), Some(872_000)),
        "the header keeps what the server committed"
    );
}

#[test]
fn an_acceptance_of_an_unknown_operation_is_not_labelled_with_this_tabs_alias() {
    let last_text = |s: &State| match &s.conversation.blocks.last().unwrap().kind {
        BlockKind::Command { text, .. } => text.clone(),
        other => panic!("{other:?}"),
    };
    // this tab asked for b; another client's switch (operation 99) is accepted
    let (mut s, _) = switching();
    model_answered(&mut s, 77, Ok(deferred()));
    let mut script = Script::new();
    model_result(&mut script, 99, true, deferred(), None);
    feed(&mut s, &script.out);
    assert_eq!(
        last_text(&s),
        "model switch accepted, applies after this turn"
    );
    assert_eq!(
        s.pending_model(),
        Some("b"),
        "this tab's switch still waits"
    );
    // before this tab's HTTP answer, its own acceptance is not named either
    let (mut s, _) = switching();
    let mut script = Script::new();
    model_result(&mut script, 77, true, deferred(), None);
    feed(&mut s, &script.out);
    assert_eq!(
        last_text(&s),
        "model switch accepted, applies after this turn"
    );
    // the alias the server names is used
    let mut s = st();
    let mut script = Script::new();
    model_result(
        &mut script,
        99,
        true,
        Some(json!({"accepted": true, "applies": "after_turn", "model": "luna"})),
        None,
    );
    feed(&mut s, &script.out);
    assert_eq!(last_text(&s), "switches to luna after this turn");
}

#[test]
fn approval_evidence_survives_live_and_replay() {
    let mut script = Script::new();
    let gate = json!({"policy_version":1,"source":"jev","verdicts":{"effect":"read_only","scope":"inside","risk":"low"}});
    script.raw("tool.pre_call",json!({"tool_call_id":"review","name":"bash","args":{"command":"ls"},"permission_gate":gate}));
    script.raw("tool.post_call",json!({"tool_call_id":"review","name":"bash","status":"ok","output":"done","metadata":{"permission_gate":gate}}));
    for _ in 0..2 {
        let mut s = State::default();
        for envelope in script.out.clone() {
            let generation = s.generation.clone();
            update(
                &mut s,
                ClientEvent::Envelope {
                    generation,
                    envelope,
                },
            )
            .unwrap();
            let block = s
                .conversation
                .blocks
                .iter()
                .find(|b| matches!(b.kind, BlockKind::ToolCall { .. }))
                .unwrap();
            assert!(withnav::tui::view::tool::header(
                &block.kind,
                0,
                &withnav::tui::theme::ThemeId::default().palette()
            )
            .spans
            .iter()
            .any(|span| span.content.contains("auto · instinct")));
        }
    }
}
